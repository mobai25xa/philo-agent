//! The interactive event loop: merges agent events and terminal input,
//! feeds the pure state machine, and performs its effects. This shell is
//! deliberately thin — everything decision-shaped lives in [`crate::app`].

use std::collections::VecDeque;
use std::sync::Arc;

use crossterm::event::{Event as TermEvent, EventStream};
use philo_agent_runtime::{AgentEvent, OperationHandle, SessionId, UserMessage};

use crate::api::host::TuiHost;
use crate::api::types::{TuiConfig, TuiExit};
use crate::app::effect::Effect;
use crate::app::state::App;
use crate::app::status::StatusData;
use crate::app::transcript::{InfoLevel, LineKind, TranscriptLine};
use crate::platform::keymap;
use crate::platform::terminal::TerminalSession;
use crate::render::frame;

use super::events::{self, Step};
use super::{host_effects, scrollback};

/// Fixed inline viewport: live row + input window + hint + status.
const VIEWPORT_HEIGHT: u16 = 8;

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
    let mut app = App::new(status);

    // Opening an existing session replays its durable context before the
    // first prompt. This uses the same read-only path as switching from the
    // session picker, so startup and interactive selection cannot drift.
    let initial_history = host_effects::execute(
        &mut app,
        host.as_ref(),
        crate::app::effect::HostRequest::SwitchSession(philo_session::SessionId::new(
            &config.session_id,
        )),
    )
    .await;
    scrollback::append_history(&mut session, &initial_history)?;

    let mut term_events = EventStream::new();
    let mut redraw_tick = tokio::time::interval(std::time::Duration::from_millis(100));
    redraw_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first interval tick is immediate; the initial frame is drawn below.
    redraw_tick.tick().await;
    // Operations queue FIFO (M6); the loop consumes the front handle's
    // events until it settles, then moves on.
    let mut handles: VecDeque<OperationHandle> = VecDeque::new();
    let exit = loop {
        // The overlay follows the channel: a queued question opens it, an
        // answered or auto-denied one closes it.
        app.sync_confirmation(host.confirmations().front());
        session.terminal.draw(|term_frame| {
            frame::draw(term_frame, &app, shift_enter);
        })?;

        let step = events::next_step(&mut handles, &mut term_events, &mut redraw_tick).await;
        let effects = match step {
            Step::Agent(Some(event)) => {
                let effects = app.on_agent_event(&event);
                if matches!(
                    event,
                    AgentEvent::CancellationRequested { .. }
                        | AgentEvent::TurnCancelled { .. }
                        | AgentEvent::OperationSettled { .. }
                ) {
                    // A cancellation or terminal event must not leave an
                    // external approval decorator waiting on a UI answer.
                    host.confirmations().deny_all();
                }
                effects
            }
            Step::Agent(None) => {
                handles.pop_front();
                app.set_busy(!handles.is_empty(), handles.len().saturating_sub(1));
                Vec::new()
            }
            Step::Term(Ok(event)) => match event {
                TermEvent::Key(key) => app.on_action(keymap::interpret(&key)),
                TermEvent::Paste(text) => app.on_paste(&text),
                TermEvent::Resize(..) => vec![Effect::Redraw],
                _ => Vec::new(),
            },
            Step::Term(Err(error)) => return Err(error),
            Step::TermClosed => break TuiExit::Normal,
            Step::Tick => vec![Effect::Redraw],
        };

        let mut quit = false;
        for effect in effects {
            match effect {
                Effect::Append(lines) => scrollback::append_history(&mut session, &lines)?,
                Effect::Submit(text) => {
                    let message = UserMessage::new(text);
                    // The live session id, not the launch one: `/new` and
                    // the picker move the prompt target.
                    match host
                        .prompt(SessionId::new(&app.status.session), message)
                        .await
                    {
                        Ok(handle) => {
                            handles.push_back(handle);
                            app.set_busy(true, handles.len().saturating_sub(1));
                        }
                        Err(error) => {
                            scrollback::append_history(
                                &mut session,
                                &[TranscriptLine {
                                    kind: LineKind::Error,
                                    text: format!("error: prompt rejected: {}", error.message()),
                                }],
                            )?;
                        }
                    }
                }
                Effect::CancelActive => {
                    if let Some(handle) = handles.front() {
                        handle.cancel();
                    }
                }
                Effect::Quit => quit = true,
                Effect::Redraw => {
                    session.terminal.clear()?;
                }
                Effect::Host(request) => {
                    let lines = host_effects::execute(&mut app, host.as_ref(), request).await;
                    scrollback::append_history(&mut session, &lines)?;
                }
            }
        }
        if quit {
            break TuiExit::Normal;
        }
    };
    drop(session);
    Ok(exit)
}
