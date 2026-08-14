//! The interactive event loop: merges agent events and terminal input,
//! feeds the pure state machine, and performs its effects. Terminal writes
//! are granted only by the frame scheduler.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event as TermEvent, EventStream};
use philo_agent_runtime::{AgentEvent, OperationHandle, SessionId};
use tokio::time::Instant;

use crate::api::host::TuiHost;
use crate::api::types::{TuiConfig, TuiExit};
use crate::app::effect::{Effect, HostRequest};
use crate::app::state::App;
use crate::app::status::StatusData;
use crate::app::transcript::InfoLevel;
use crate::platform::clipboard::ClipboardContent;
use crate::platform::keymap;
use crate::platform::terminal::TerminalSession;
use crate::render::markdown::MarkdownRenderer;

use super::events::{self, AgentItem, Step};
use super::output::{FlushReport, PendingOutput};
use super::scheduler::FrameScheduler;
use super::tasks::{PendingTasks, SubmissionResult, TaskCompletion};
use super::{host_effects, media, tasks};

/// Fixed inline viewport: activity + live tail + popover + composer + status.
const VIEWPORT_HEIGHT: u16 = crate::render::frame::VIEWPORT_HEIGHT;
/// ConfirmationChannel currently has no wake stream. While an operation is
/// active, poll its front without producing a frame unless the overlay changed.
const CONFIRMATION_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Runs the interactive session until the user quits.
///
/// Terminal ownership: raw mode and the inline viewport are held for the
/// whole call and restored on every exit path (the guard also covers
/// panics). Errors after the loop starts surface as `io::Error`.
pub async fn run(host: Arc<dyn TuiHost>, config: TuiConfig) -> std::io::Result<TuiExit> {
    let mut session = TerminalSession::enter(VIEWPORT_HEIGHT)?;
    let shift_enter = session.shift_enter;

    let mut status = StatusData::new(
        &config.model_name,
        &config.session_id,
        if config.verbose {
            InfoLevel::Verbose
        } else {
            InfoLevel::Default
        },
    );
    status.context_window = config.context_window;
    let mut app = App::new(status, config.show_reasoning);
    let mut markdown = MarkdownRenderer::new();

    let start = Instant::now();
    let mut scheduler = FrameScheduler::new(start);
    let mut output = PendingOutput::default();
    let mut tasks = PendingTasks::new(Arc::clone(&host));
    // Durable history is loaded as the first owned host task. The initial
    // panel and terminal input remain live while a slow store responds.
    tasks.start_host(HostRequest::SwitchSession(philo_session::SessionId::new(
        &config.session_id,
    )));
    let mut term_events = EventStream::new();
    // Operations queue FIFO (M6); the loop consumes the front handle's
    // events until it settles, then moves on.
    let mut handles: VecDeque<OperationHandle> = VecDeque::new();
    let mut compaction: Option<events::CompactionFuture> = None;
    let mut exit_requested = false;

    let exit = loop {
        let now = Instant::now();
        if app.sync_confirmation(host.confirmations().front()) {
            scheduler.invalidate_immediate(now);
        }
        scheduler.sync_animation(app.animation_active(), now);

        let report = output.flush(
            &mut session.terminal,
            &app,
            &mut markdown,
            shift_enter,
            &mut scheduler,
            now,
        )?;
        assert_single_round_writes(report);
        if exit_requested {
            break TuiExit::Normal;
        }

        let confirmation_poll = (!handles.is_empty()).then_some(now + CONFIRMATION_POLL_INTERVAL);
        let step = events::next_step(
            &mut handles,
            &mut tasks,
            &mut compaction,
            &mut term_events,
            scheduler.frame_deadline(),
            scheduler.animation_deadline(),
            confirmation_poll,
        )
        .await;
        let event_time = Instant::now();

        let effects = match step {
            Step::Agent(first) => {
                let mut effects = Vec::new();
                for item in events::drain_ready_agent_items(&mut handles, first) {
                    if let AgentItem::Event(event) = item {
                        effects.extend(app.on_agent_event(&event));
                        if is_terminal_operation_event(&event) {
                            // A cancellation or terminal event must not leave
                            // an external approval decorator waiting.
                            host.confirmations().deny_all();
                        }
                    }
                }
                sync_busy(&mut app, &handles, &tasks, compaction.is_some());
                scheduler.invalidate_background(event_time);
                effects
            }
            Step::Task(completion) => {
                let effects = match completion {
                    TaskCompletion::Host(result) => {
                        if result.resets_session() {
                            markdown.reset();
                        }
                        host_effects::apply(&mut app, result)
                    }
                    TaskCompletion::Clipboard(result) => finish_clipboard(&mut app, result),
                    TaskCompletion::Submission(SubmissionResult::Accepted(handle)) => {
                        handles.push_back(handle);
                        Vec::new()
                    }
                    TaskCompletion::Submission(SubmissionResult::Rejected(error)) => {
                        vec![Effect::Append(tasks::task_error(error))]
                    }
                    TaskCompletion::Submission(SubmissionResult::MediaRefused {
                        text,
                        kept,
                        errors,
                        draft_generation,
                    }) => {
                        let restored = app.restore_draft_if_current(draft_generation, &text, kept);
                        vec![Effect::Append(media::refusal_lines_for_restore(
                            &errors, restored,
                        ))]
                    }
                    TaskCompletion::Failed(error) => {
                        vec![Effect::Append(tasks::task_error(error))]
                    }
                    TaskCompletion::Superseded => Vec::new(),
                };
                tasks.resume_submissions(SessionId::new(&app.status.session));
                sync_busy(&mut app, &handles, &tasks, compaction.is_some());
                scheduler.invalidate_background(event_time);
                effects
            }
            Step::Compaction(result) => {
                compaction.take();
                let effects = app.finish_manual_compaction(result);
                sync_busy(&mut app, &handles, &tasks, false);
                scheduler.invalidate_background(event_time);
                effects
            }
            Step::Term(Ok(event)) => match event {
                TermEvent::Key(key) => {
                    scheduler.invalidate_immediate(event_time);
                    let action = keymap::interpret(&key);
                    if matches!(action, crate::app::action::Action::Escape) {
                        tasks.cancel_transient();
                        tasks.resume_submissions(SessionId::new(&app.status.session));
                    }
                    app.on_action(action)
                }
                TermEvent::Paste(text) => {
                    scheduler.invalidate_immediate(event_time);
                    app.on_paste(&text)
                }
                TermEvent::Resize(..) => {
                    // Ratatui autoresizes on the next draw. A resize is an
                    // immediate invalidation, not an unconditional clear.
                    scheduler.invalidate_immediate(event_time);
                    Vec::new()
                }
                _ => Vec::new(),
            },
            Step::Term(Err(error)) => return Err(error),
            Step::TermClosed => {
                // Flush any completed history already accepted by the App
                // before restoring the terminal.
                scheduler.invalidate_immediate(event_time);
                exit_requested = true;
                Vec::new()
            }
            Step::FrameDeadline | Step::ConfirmationPoll => Vec::new(),
            Step::AnimationDeadline => {
                if scheduler.take_animation_tick(event_time) && app.on_tick() {
                    scheduler.invalidate_background(event_time);
                }
                Vec::new()
            }
        };

        let mut quit = false;
        // Completed host results can request follow-up work (opening the picker
        // requests its first preview), so the queue is drained rather than
        // iterated. Transcript lines stay queued until the next granted frame
        // and are then inserted in one scrollback batch.
        let mut pending: VecDeque<Effect> = effects.into();
        while let Some(effect) = pending.pop_front() {
            match effect {
                Effect::Append(lines) => {
                    output.append(&mut markdown, lines);
                    scheduler.invalidate_background(event_time);
                }
                Effect::Submit { text, attachments } => {
                    let draft_generation = app.draft_generation();
                    tasks.enqueue_submission(
                        SessionId::new(&app.status.session),
                        text,
                        attachments,
                        draft_generation,
                    );
                    sync_busy(&mut app, &handles, &tasks, compaction.is_some());
                    scheduler.invalidate_immediate(Instant::now());
                }
                Effect::ReadClipboard => {
                    tasks.start_clipboard();
                }
                Effect::CancelActive => {
                    if let Some(handle) = handles.front() {
                        handle.cancel();
                    } else {
                        tasks.cancel_submissions();
                        sync_busy(&mut app, &handles, &tasks, compaction.is_some());
                    }
                }
                Effect::StartCompaction => {
                    debug_assert!(compaction.is_none(), "only one manual compaction may run");
                    compaction = Some(host.compact(SessionId::new(&app.status.session)));
                }
                Effect::CancelCompaction => {
                    compaction.take();
                    sync_busy(&mut app, &handles, &tasks, false);
                }
                Effect::Quit => quit = true,
                Effect::HardRedraw => {
                    scheduler.request_hard_redraw(Instant::now());
                }
                Effect::Host(request) => {
                    if let HostRequest::Respond(id, response) = request {
                        host.confirmations().respond(id, response);
                    } else {
                        tasks.start_host(request);
                        tasks.resume_submissions(SessionId::new(&app.status.session));
                    }
                    scheduler.invalidate_immediate(Instant::now());
                }
            }
        }
        if quit {
            // Preserve any append emitted earlier in this effect queue.
            scheduler.invalidate_immediate(Instant::now());
            exit_requested = true;
        }
    };
    drop(session);
    Ok(exit)
}

fn assert_single_round_writes(report: FlushReport) {
    debug_assert!(report.clears <= 1);
    debug_assert!(report.inserts <= 1);
    debug_assert!(report.draws <= 1);
    debug_assert!(report.inserts == 0 || report.draws == 1);
}

fn is_terminal_operation_event(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::CancellationRequested { .. }
            | AgentEvent::TurnCancelled { .. }
            | AgentEvent::OperationSettled { .. }
    )
}

fn sync_busy(
    app: &mut App,
    handles: &VecDeque<OperationHandle>,
    tasks: &PendingTasks,
    maintenance_active: bool,
) {
    let operations = handles.len() + tasks.submission_count();
    let queued = if maintenance_active {
        operations
    } else {
        operations.saturating_sub(1)
    };
    app.set_busy(operations > 0, queued);
}

/// `Ctrl+V` when the terminal did not turn it into a bracketed paste: an
/// image joins the pending attachments, text lands in the draft, and any
/// failure degrades to a hint without disturbing the input.
fn finish_clipboard(app: &mut App, result: Result<ClipboardContent, String>) -> Vec<Effect> {
    match result {
        Ok(ClipboardContent::Image { media_type, bytes }) => {
            app.attach_image(media_type, bytes, "clipboard image")
        }
        Ok(ClipboardContent::Text(text)) => app.on_paste(&text),
        Ok(ClipboardContent::Empty) => app.clipboard_unavailable("it holds no image or text"),
        Err(reason) => app.clipboard_unavailable(&reason),
    }
}
