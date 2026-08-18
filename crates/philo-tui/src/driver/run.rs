//! The interactive event loop: merges frontend updates and terminal input,
//! feeds the pure state machine, and performs its effects. Terminal writes
//! are granted only by the frame scheduler.

use std::collections::VecDeque;

use philo_agent_service::{
    CommandDispatch, FrontendAvailability, FrontendClient, FrontendCommand,
    FrontendMaintenancePhase, FrontendRequestId, FrontendRevision, FrontendSnapshot,
    FrontendUpdate, FrontendUpdateKind, ServiceHealth,
};
use ratatui::Terminal;
use ratatui::backend::Backend;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::api::types::{TuiLaunchConfig, TuiOutcome, TuiRunReport, TuiScreen};
use crate::app::action::Action;
use crate::app::effect::{Effect, HostRequest};
use crate::app::overlay::Preview;
use crate::app::state::{App, SessionLoadIntent};
use crate::app::status::StatusData;
use crate::app::submit::{CancelDispatchResult, SubmitDispatchResult};
use crate::app::transcript::{InfoLevel, LineKind, TranscriptLine};
use crate::platform::clipboard::ClipboardContent;
use crate::platform::input::{
    CrosstermInputSource, InputFaultTracker, TerminalInput, TerminalInputFault, TerminalInputSource,
};
use crate::platform::keymap;
use crate::platform::terminal::TerminalSession;
use crate::render::markdown::MarkdownRenderer;

use super::events::{self, Step};
use super::interrupt::{self, CtrlCDecision, CtrlCPhase};
use super::output::{FlushReport, PendingOutput};
use super::scheduler::FrameScheduler;
use super::tasks;
use super::tasks::{MediaResult, PendingTasks, TaskCompletion};

const DRAW_FAILURE_BUDGET: u32 = 3;
const INPUT_ERROR_BUDGET: u32 = 8;
const SNAPSHOT_REQUEST_RETRY_BUDGET: u8 = 3;
const SESSION_LOAD_RETRY_BUDGET: u8 = 3;

/// Production entry: enter the terminal, drive the loop, restore on every path.
pub async fn run_async(client: FrontendClient, config: TuiLaunchConfig) -> TuiRunReport {
    let screen = config.screen;
    let mut session = match TerminalSession::enter(screen) {
        Ok(session) => session,
        Err(error) => {
            return TuiRunReport {
                outcome: TuiOutcome::FallbackRequested {
                    fault: error.fault.to_string(),
                },
                restore: error.restore,
            };
        }
    };
    let shift_enter = session.shift_enter;
    let mut input = CrosstermInputSource::new();
    let outcome = run_loop(
        client,
        config,
        &mut session.terminal,
        &mut input,
        shift_enter,
        screen,
    )
    .await;
    let restore = session.finish();
    TuiRunReport { outcome, restore }
}

/// Alias of [`run_async`]. The production path is `run_async` only.
pub async fn run(client: FrontendClient, config: TuiLaunchConfig) -> TuiRunReport {
    run_async(client, config).await
}

struct FrontendSync {
    epoch: Option<philo_agent_service::FrontendEpoch>,
    revision: FrontendRevision,
    awaiting_snapshot: bool,
    want_snapshot: bool,
    snapshot_retries: u8,
    snapshot_request: Option<FrontendRequestId>,
    preview_request: Option<FrontendRequestId>,
    preview_session_id: Option<String>,
    model_request: Option<FrontendRequestId>,
}

impl FrontendSync {
    fn new() -> Self {
        Self {
            epoch: None,
            revision: FrontendRevision::ZERO,
            awaiting_snapshot: false,
            want_snapshot: false,
            snapshot_retries: 0,
            snapshot_request: None,
            preview_request: None,
            preview_session_id: None,
            model_request: None,
        }
    }

    fn accept(&mut self, update: &FrontendUpdate) -> bool {
        if let Some(epoch) = self.epoch {
            if update.epoch < epoch {
                return false;
            }
            if update.epoch > epoch {
                self.epoch = Some(update.epoch);
                self.revision = update.revision;
                self.awaiting_snapshot = false;
                return true;
            }
        } else {
            self.epoch = Some(update.epoch);
        }
        if update.revision < self.revision {
            return false;
        }
        if self.awaiting_snapshot && !allowed_during_snapshot_wait(&update.kind) {
            return false;
        }
        if let Some(id) = update.request_id {
            if matches!(update.kind, FrontendUpdateKind::SessionPreviewed { .. })
                && self.preview_request.is_some_and(|expected| expected != id)
            {
                return false;
            }
            if matches!(
                update.kind,
                FrontendUpdateKind::GenerationInstalled { .. }
                    | FrontendUpdateKind::GenerationInstallFailed { .. }
            ) && self.model_request.is_some_and(|expected| expected != id)
            {
                return false;
            }
            if matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
                && self.snapshot_request.is_some_and(|expected| expected != id)
            {
                return false;
            }
        }
        self.revision = update.revision;
        if matches!(update.kind, FrontendUpdateKind::SnapshotReady(_)) {
            self.awaiting_snapshot = false;
        }
        true
    }
}

fn allowed_during_snapshot_wait(kind: &FrontendUpdateKind) -> bool {
    matches!(
        kind,
        FrontendUpdateKind::SnapshotReady(_)
            | FrontendUpdateKind::ResyncRequired { .. }
            | FrontendUpdateKind::SubmitAccepted { .. }
            | FrontendUpdateKind::CommandRejected { .. }
            | FrontendUpdateKind::ServiceHealthChanged { .. }
    )
}

struct LoopState {
    client: FrontendClient,
    sync: FrontendSync,
    preview_generation: u64,
    active_operation_id: Option<String>,
    maintenance_id: Option<String>,
    ctrl_c: CtrlCPhase,
    next_cancel_request: u64,
    /// Maps enqueued submit request ids to local intents.
    submit_requests: Vec<(FrontendRequestId, u64)>,
    pending_session_load: Option<String>,
    session_load_retries: u8,
}

impl LoopState {
    fn send(&self, command: FrontendCommand) -> CommandDispatch<FrontendRequestId> {
        self.client.try_command(command)
    }

    fn next_cancel_request(&mut self) -> u64 {
        let id = self.next_cancel_request;
        self.next_cancel_request = self.next_cancel_request.wrapping_add(1).max(1);
        id
    }

    fn remember_submit(&mut self, request_id: FrontendRequestId, intent_id: u64) {
        self.submit_requests.retain(|(id, _)| *id != request_id);
        self.submit_requests.push((request_id, intent_id));
    }

    fn take_submit_intent(&mut self, request_id: FrontendRequestId) -> Option<u64> {
        if let Some(index) = self
            .submit_requests
            .iter()
            .position(|(id, _)| *id == request_id)
        {
            return Some(self.submit_requests.remove(index).1);
        }
        None
    }
}

fn apply_snapshot_request_dispatch(
    sync: &mut FrontendSync,
    dispatch: CommandDispatch<FrontendRequestId>,
) -> Result<Vec<Effect>, TuiOutcome> {
    match dispatch {
        CommandDispatch::Enqueued(id) => {
            sync.snapshot_request = Some(id);
            sync.awaiting_snapshot = true;
            sync.want_snapshot = false;
            Ok(Vec::new())
        }
        CommandDispatch::Backpressured => {
            sync.awaiting_snapshot = false;
            if sync.snapshot_retries == 0 {
                sync.want_snapshot = false;
            } else {
                sync.snapshot_retries = sync.snapshot_retries.saturating_sub(1);
                sync.want_snapshot = true;
            }
            Ok(vec![notice_effect("服务繁忙，快照请求未发送")])
        }
        CommandDispatch::Disconnected { lane } => Err(TuiOutcome::FrontendRestartRequested {
            fault: format!("frontend disconnected on snapshot ({lane})"),
        }),
    }
}

fn try_request_snapshot(state: &mut LoopState) -> Result<Vec<Effect>, TuiOutcome> {
    if !state.sync.want_snapshot || state.sync.awaiting_snapshot {
        return Ok(Vec::new());
    }
    let revision = state.sync.revision;
    apply_snapshot_request_dispatch(&mut state.sync, state.client.request_snapshot(revision))
}

fn try_load_session(state: &mut LoopState) -> Result<Vec<Effect>, TuiOutcome> {
    let Some(session_id) = state.pending_session_load.clone() else {
        return Ok(Vec::new());
    };
    match state.send(FrontendCommand::LoadSession { session_id }) {
        CommandDispatch::Enqueued(_) => {
            state.pending_session_load = None;
            Ok(Vec::new())
        }
        CommandDispatch::Backpressured => {
            if state.session_load_retries == 0 {
                state.pending_session_load = None;
            } else {
                state.session_load_retries = state.session_load_retries.saturating_sub(1);
            }
            Ok(vec![notice_effect("服务繁忙，会话加载未发送")])
        }
        CommandDispatch::Disconnected { lane } => Err(TuiOutcome::FrontendRestartRequested {
            fault: format!("frontend disconnected on load ({lane})"),
        }),
    }
}

fn begin_snapshot_resync(state: &mut LoopState) -> Result<Vec<Effect>, TuiOutcome> {
    state.sync.want_snapshot = true;
    state.sync.snapshot_retries = SNAPSHOT_REQUEST_RETRY_BUDGET;
    try_request_snapshot(state)
}

fn cancel_dispatch_result(dispatch: CommandDispatch<FrontendRequestId>) -> CancelDispatchResult {
    match dispatch {
        CommandDispatch::Enqueued(id) => CancelDispatchResult::Enqueued(id),
        CommandDispatch::Backpressured => CancelDispatchResult::Backpressured,
        CommandDispatch::Disconnected { lane } => CancelDispatchResult::Disconnected { lane },
    }
}

fn apply_submit_accepted(
    state: &mut LoopState,
    app: &mut App,
    request_id: Option<FrontendRequestId>,
    operation_id: String,
) -> Vec<Effect> {
    if let Some(id) = request_id
        && let Some(intent_id) = state.take_submit_intent(id)
    {
        return app.on_action(Action::SubmitAccepted {
            intent_id,
            operation_id,
        });
    }
    let Some(pending) = app.submit_state().pending() else {
        return Vec::new();
    };
    let intent_id = pending.intent_id;
    if pending.request_id == request_id {
        return app.on_action(Action::SubmitAccepted {
            intent_id,
            operation_id,
        });
    }
    Vec::new()
}

fn notice_effect(text: impl Into<String>) -> Effect {
    Effect::Append(vec![TranscriptLine {
        kind: LineKind::Notice,
        text: text.into(),
    }])
}

fn ctrl_c_decision(state: &mut LoopState, treat_unknown_as_busy: bool) -> CtrlCDecision {
    if let Some(id) = state.active_operation_id.clone() {
        state.ctrl_c.observe_busy(id);
    } else if treat_unknown_as_busy && matches!(state.ctrl_c, CtrlCPhase::Idle) {
        state.ctrl_c = CtrlCPhase::Busy { operation_id: None };
    }
    let cancel_request = state.next_cancel_request();
    state.ctrl_c.on_ctrl_c(cancel_request)
}

fn apply_cancel_dispatch(
    state: &mut LoopState,
    app: &mut App,
    cancel_request: u64,
    result: CancelDispatchResult,
) -> Vec<Effect> {
    let notice = state
        .ctrl_c
        .on_cancel_dispatch_finished(cancel_request, result.clone());
    app.on_action(Action::CancelDispatchFinished {
        request_id: cancel_request,
        result,
    });
    if let Some(notice) = notice {
        app.ingest_appends(vec![notice_effect(notice)])
    } else {
        Vec::new()
    }
}

fn dispatch_cancel_operation(
    state: &mut LoopState,
    app: &mut App,
    operation_id: String,
    cancel_request: u64,
) -> (Vec<Effect>, Option<TuiOutcome>) {
    let result =
        cancel_dispatch_result(state.send(FrontendCommand::CancelOperation { operation_id }));
    let disconnected = matches!(&result, CancelDispatchResult::Disconnected { .. });
    let lane = match &result {
        CancelDispatchResult::Disconnected { lane } => Some(*lane),
        _ => None,
    };
    let effects = apply_cancel_dispatch(state, app, cancel_request, result);
    let outcome = disconnected.then(|| TuiOutcome::FrontendRestartRequested {
        fault: format!(
            "frontend disconnected on cancel ({})",
            lane.unwrap_or("unknown")
        ),
    });
    (effects, outcome)
}

fn apply_ctrl_c_decision(
    state: &mut LoopState,
    app: &mut App,
    exit_requested: &mut Option<TuiOutcome>,
    decision: CtrlCDecision,
) -> Vec<Effect> {
    match decision {
        CtrlCDecision::UserExit => {
            *exit_requested = Some(TuiOutcome::UserExit);
            Vec::new()
        }
        CtrlCDecision::Cancel {
            operation_id,
            cancel_request,
        } => {
            let (effects, outcome) =
                dispatch_cancel_operation(state, app, operation_id, cancel_request);
            if let Some(outcome) = outcome {
                *exit_requested = Some(outcome);
            }
            effects
        }
        CtrlCDecision::WaitForId { .. } => Vec::new(),
        CtrlCDecision::ForcedExit { code } => {
            *exit_requested = Some(TuiOutcome::ForcedExitRequested { code });
            Vec::new()
        }
    }
}

fn apply_interrupt_pulses(
    state: &mut LoopState,
    app: &mut App,
    interrupt: Option<&mut watch::Receiver<u64>>,
    seen: &mut u64,
    exit_requested: &mut Option<TuiOutcome>,
) -> Vec<Effect> {
    let Some(rx) = interrupt else {
        return Vec::new();
    };
    let delta = interrupt::take_pulses(rx, seen);
    let mut effects = Vec::new();
    for _ in 0..delta {
        if exit_requested.is_some() {
            break;
        }
        let decision = ctrl_c_decision(state, app.status.busy);
        effects.extend(apply_ctrl_c_decision(state, app, exit_requested, decision));
    }
    effects
}

/// Testable event loop. Does not own or restore a real terminal session.
pub(crate) async fn run_loop<B: Backend>(
    client: FrontendClient,
    config: TuiLaunchConfig,
    terminal: &mut Terminal<B>,
    input: &mut impl TerminalInputSource,
    shift_enter: bool,
    screen: TuiScreen,
) -> TuiOutcome {
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
    let mut tasks = PendingTasks::new();
    let mut input_faults = InputFaultTracker::new(INPUT_ERROR_BUDGET);
    let mut pending_zero_resize = false;
    let mut draw_failures: u32 = 0;
    let mut exit_requested = None;
    let mut rebuild_deadline = None;

    let mut state = LoopState {
        client,
        sync: FrontendSync::new(),
        preview_generation: 0,
        active_operation_id: None,
        maintenance_id: None,
        ctrl_c: CtrlCPhase::Idle,
        next_cancel_request: 1,
        submit_requests: Vec::new(),
        pending_session_load: None,
        session_load_retries: 0,
    };
    let mut interrupt = config.interrupt;
    let mut interrupt_seen = 0u64;
    if let Some(rx) = interrupt.as_mut() {
        interrupt_seen = *rx.borrow_and_update();
    }

    if config.session_id.is_empty() {
        state.sync.revision = FrontendRevision::ZERO;
        if let Err(outcome) =
            begin_snapshot_resync(&mut state).map(|effects| app.ingest_appends(effects))
        {
            return outcome;
        }
    } else {
        app.expect_session_load(SessionLoadIntent::Switch);
        state.pending_session_load = Some(config.session_id.clone());
        state.session_load_retries = SESSION_LOAD_RETRY_BUDGET;
        match try_load_session(&mut state) {
            Ok(effects) => {
                app.ingest_appends(effects);
            }
            Err(outcome) => return outcome,
        }
    }

    loop {
        let now = Instant::now();
        scheduler.sync_animation(app.animation_active(), now);

        if !pending_zero_resize {
            let report = output.flush(
                terminal,
                &app,
                &mut markdown,
                shift_enter,
                &mut scheduler,
                now,
            );
            assert_single_round_writes(report);
            if report.failed {
                draw_failures = draw_failures.saturating_add(1);
                if draw_failures >= DRAW_FAILURE_BUDGET {
                    return TuiOutcome::FrontendRestartRequested {
                        fault: "draw failed repeatedly".to_owned(),
                    };
                }
            } else {
                draw_failures = 0;
            }
        }
        if let Some(outcome) = exit_requested {
            return outcome;
        }

        match try_request_snapshot(&mut state) {
            Ok(effects) => {
                if !effects.is_empty() {
                    app.ingest_appends(effects);
                    scheduler.invalidate_background(now);
                }
            }
            Err(outcome) => return outcome,
        }
        match try_load_session(&mut state) {
            Ok(effects) => {
                if !effects.is_empty() {
                    app.ingest_appends(effects);
                    scheduler.invalidate_background(now);
                }
            }
            Err(outcome) => return outcome,
        }

        let step = events::next_step(
            &state.client,
            &mut tasks,
            input,
            scheduler.frame_deadline(),
            scheduler.animation_deadline(),
            rebuild_deadline,
            interrupt.as_mut(),
        )
        .await;
        let event_time = Instant::now();

        let effects = match step {
            Step::Update(first) => {
                let updates = events::drain_ready_updates(&state.client, first);
                apply_updates(&mut app, &mut markdown, &mut state, updates).unwrap_or_else(
                    |outcome| {
                        exit_requested = Some(outcome);
                        Vec::new()
                    },
                )
            }
            Step::UpdatesDisconnected => {
                exit_requested = Some(TuiOutcome::FrontendRestartRequested {
                    fault: "frontend update stream closed".to_owned(),
                });
                Vec::new()
            }
            Step::Task(completion) => {
                match apply_task_with_submit(&mut app, &mut state, completion) {
                    Ok(effects) => effects,
                    Err(outcome) => {
                        exit_requested = Some(outcome);
                        Vec::new()
                    }
                }
            }
            Step::Input(Ok(event)) => apply_input(
                &mut app,
                &mut tasks,
                &mut scheduler,
                screen,
                event_time,
                &mut pending_zero_resize,
                event,
            ),
            Step::Input(Err(fault)) => {
                match handle_input_fault(
                    fault,
                    &mut input_faults,
                    &mut pending_zero_resize,
                    event_time,
                ) {
                    Ok(InputFaultAction::Continue) => Vec::new(),
                    Ok(InputFaultAction::ScheduleRebuild(deadline)) => {
                        app.status.input_rebuilding = true;
                        rebuild_deadline = Some(deadline);
                        scheduler.invalidate_immediate(event_time);
                        Vec::new()
                    }
                    Err(outcome) => {
                        exit_requested = Some(outcome);
                        Vec::new()
                    }
                }
            }
            Step::InputClosed => {
                exit_requested = Some(TuiOutcome::FrontendRestartRequested {
                    fault: "terminal input stream terminated".to_owned(),
                });
                Vec::new()
            }
            Step::InputRebuildDue => {
                rebuild_deadline = None;
                match input.rebuild() {
                    Ok(()) => {
                        app.status.input_rebuilding = false;
                        scheduler.invalidate_immediate(event_time);
                    }
                    Err(error) => {
                        exit_requested = Some(TuiOutcome::FallbackRequested {
                            fault: format!("input rebuild failed: {error:?}"),
                        });
                    }
                }
                Vec::new()
            }
            Step::FrameDeadline => Vec::new(),
            Step::AnimationDeadline => {
                if scheduler.take_animation_tick(event_time) && app.on_tick() {
                    scheduler.invalidate_background(event_time);
                }
                Vec::new()
            }
            Step::Interrupt => apply_interrupt_pulses(
                &mut state,
                &mut app,
                interrupt.as_mut(),
                &mut interrupt_seen,
                &mut exit_requested,
            ),
        };

        let mut pending: VecDeque<Effect> = effects.into();
        while let Some(effect) = pending.pop_front() {
            match effect {
                Effect::Append(_) => {
                    scheduler.invalidate_background(event_time);
                }
                Effect::PrepareSubmit {
                    intent_id,
                    text,
                    attachments,
                } => {
                    tasks.enqueue_media(intent_id, text, attachments);
                    scheduler.invalidate_immediate(Instant::now());
                }
                Effect::ReadClipboard => tasks.start_clipboard(),
                Effect::WriteClipboard(text) => tasks.start_clipboard_write(text),
                Effect::CancelActive => {
                    if let Some(operation_id) = state.active_operation_id.clone() {
                        let cancel_request = state.next_cancel_request();
                        state.ctrl_c = CtrlCPhase::CancelDispatching {
                            operation_id: Some(operation_id.clone()),
                            cancel_request,
                        };
                        let (cancel_effects, outcome) = dispatch_cancel_operation(
                            &mut state,
                            &mut app,
                            operation_id,
                            cancel_request,
                        );
                        pending.extend(cancel_effects);
                        if let Some(outcome) = outcome {
                            exit_requested = Some(outcome);
                        }
                    } else {
                        tasks.cancel_media();
                        if let Some(submission) = app.submit_state().pending().cloned() {
                            pending.extend(app.on_action(Action::SubmitMediaRefused {
                                intent_id: submission.intent_id,
                                kept: submission.attachments,
                                errors: vec!["cancelled".to_owned()],
                            }));
                        }
                    }
                }
                Effect::InterruptCancel => {
                    if state.active_operation_id.is_none() && !app.status.busy {
                        tasks.cancel_media();
                        if let Some(submission) = app.submit_state().pending().cloned() {
                            pending.extend(app.on_action(Action::SubmitMediaRefused {
                                intent_id: submission.intent_id,
                                kept: submission.attachments,
                                errors: vec!["cancelled".to_owned()],
                            }));
                        }
                    } else {
                        let decision = ctrl_c_decision(&mut state, true);
                        pending.extend(apply_ctrl_c_decision(
                            &mut state,
                            &mut app,
                            &mut exit_requested,
                            decision,
                        ));
                    }
                }
                Effect::StartCompaction => {
                    match state.send(FrontendCommand::StartCompaction {
                        session_id: app.status.session.clone(),
                    }) {
                        CommandDispatch::Enqueued(_) => {}
                        CommandDispatch::Backpressured => {
                            pending.extend(
                                app.ingest_appends(vec![notice_effect("服务繁忙，压缩请求未发送")]),
                            );
                        }
                        CommandDispatch::Disconnected { lane } => {
                            exit_requested = Some(TuiOutcome::FrontendRestartRequested {
                                fault: format!("frontend disconnected on compaction ({lane})"),
                            });
                        }
                    }
                }
                Effect::CancelCompaction => {
                    if let Some(maintenance_id) = state.maintenance_id.clone() {
                        let result = cancel_dispatch_result(
                            state.send(FrontendCommand::CancelMaintenance { maintenance_id }),
                        );
                        match &result {
                            CancelDispatchResult::Disconnected { lane } => {
                                exit_requested = Some(TuiOutcome::FrontendRestartRequested {
                                    fault: format!(
                                        "frontend disconnected on cancel maintenance ({lane})"
                                    ),
                                });
                            }
                            _ => {
                                pending.extend(app.on_action(
                                    Action::CompactionCancelDispatchFinished { result },
                                ));
                            }
                        }
                    } else {
                        pending.extend(app.ingest_appends(vec![notice_effect("取消请求未发送")]));
                    }
                }
                Effect::Quit => {
                    scheduler.invalidate_immediate(Instant::now());
                    exit_requested = Some(TuiOutcome::UserExit);
                }
                Effect::RequestShutdown => {
                    match state.send(FrontendCommand::ShutdownRequested) {
                        CommandDispatch::Enqueued(_) => {
                            exit_requested = Some(TuiOutcome::ProcessShutdownRequested);
                        }
                        CommandDispatch::Backpressured => {
                            pending.extend(
                                app.ingest_appends(vec![notice_effect("服务繁忙，关闭请求未发送")]),
                            );
                            // Still escalate: user confirmed quit during busy turn.
                            exit_requested = Some(TuiOutcome::ProcessShutdownRequested);
                        }
                        CommandDispatch::Disconnected { .. } => {
                            exit_requested = Some(TuiOutcome::ProcessShutdownRequested);
                        }
                    }
                }
                Effect::HardRedraw => {
                    scheduler.request_hard_redraw(Instant::now());
                }
                Effect::Host(request) => {
                    if let Some(outcome) = dispatch_host(&mut state, &mut app, request) {
                        exit_requested = Some(outcome);
                    }
                    scheduler.invalidate_immediate(Instant::now());
                }
            }
        }
    }
}

fn apply_updates(
    app: &mut App,
    markdown: &mut MarkdownRenderer,
    state: &mut LoopState,
    updates: Vec<FrontendUpdate>,
) -> Result<Vec<Effect>, TuiOutcome> {
    let mut effects = Vec::new();
    for update in updates {
        if !state.sync.accept(&update) {
            continue;
        }
        let (tracked, outcome) = track_identities(state, app, &update);
        effects.extend(tracked);
        if let Some(outcome) = outcome {
            return Err(outcome);
        }
        if matches!(
            update.kind,
            FrontendUpdateKind::SessionLoaded { .. } | FrontendUpdateKind::SnapshotReady(_)
        ) {
            markdown.reset();
        }
        if let FrontendUpdateKind::ResyncRequired { .. } = &update.kind {
            effects.extend(app.ingest_appends(begin_snapshot_resync(state)?));
            continue;
        }
        if let FrontendUpdateKind::CommandRejected { reason } = &update.kind
            && let Some(expected) = state.sync.preview_request
            && update.request_id == Some(expected)
            && let Some(session_id) = state.sync.preview_session_id.take()
        {
            state.sync.preview_request = None;
            app.set_preview(&session_id, Preview::Failed(reason.to_string()));
            continue;
        }
        if let FrontendUpdateKind::SubmitAccepted { operation_id, .. } = &update.kind {
            effects.extend(apply_submit_accepted(
                state,
                app,
                update.request_id,
                operation_id.clone(),
            ));
            continue;
        }
        if let FrontendUpdateKind::CommandRejected { reason } = &update.kind {
            if let Some(request_id) = update.request_id
                && let Some(intent_id) = state.take_submit_intent(request_id)
            {
                effects.extend(app.on_action(Action::SubmitCommandRejected {
                    intent_id,
                    reason: reason.clone(),
                }));
                continue;
            }
            // Unique pending without request_id match: still restore.
            if let Some(intent_id) = app.submit_state().intent_id()
                && app
                    .submit_state()
                    .pending()
                    .is_some_and(|pending| pending.request_id == update.request_id)
            {
                effects.extend(app.on_action(Action::SubmitCommandRejected {
                    intent_id,
                    reason: reason.clone(),
                }));
                continue;
            }
        }
        effects.extend(app.apply_update(&update));
    }
    Ok(effects)
}

fn track_identities(
    state: &mut LoopState,
    app: &mut App,
    update: &FrontendUpdate,
) -> (Vec<Effect>, Option<TuiOutcome>) {
    match &update.kind {
        FrontendUpdateKind::OperationAccepted { operation_id, .. } => {
            let waiting = matches!(
                state.ctrl_c,
                CtrlCPhase::CancelDispatching {
                    operation_id: None,
                    ..
                }
            );
            state.active_operation_id = Some(operation_id.clone());
            state.ctrl_c.observe_busy(operation_id.clone());
            if waiting {
                if let Some(id) = state.ctrl_c.pending_cancel_id().map(str::to_owned) {
                    let cancel_request = state
                        .ctrl_c
                        .cancel_request()
                        .unwrap_or_else(|| state.next_cancel_request());
                    return dispatch_cancel_operation(state, app, id, cancel_request);
                }
            }
            (Vec::new(), None)
        }
        FrontendUpdateKind::AvailabilityChanged { availability, .. } => {
            match availability {
                FrontendAvailability::Busy { operation_id } => {
                    state.active_operation_id = Some(operation_id.clone());
                    state.ctrl_c.observe_busy(operation_id.clone());
                }
                FrontendAvailability::Idle => {
                    state.active_operation_id = None;
                    state.maintenance_id = None;
                    state.ctrl_c.observe_idle();
                }
                FrontendAvailability::Compacting { .. } => {}
            }
            (Vec::new(), None)
        }
        FrontendUpdateKind::MaintenanceChanged(maintenance) => {
            state.maintenance_id = Some(maintenance.id.clone());
            (Vec::new(), None)
        }
        FrontendUpdateKind::SnapshotReady(snapshot) => {
            track_snapshot_identities(state, app, snapshot)
        }
        FrontendUpdateKind::ServiceHealthChanged { health } => {
            if matches!(health, ServiceHealth::RuntimeEpochEnded { .. }) {
                state.active_operation_id = None;
                state.maintenance_id = None;
                state.ctrl_c.observe_idle();
            }
            (Vec::new(), None)
        }
        _ => (Vec::new(), None),
    }
}

fn track_snapshot_identities(
    state: &mut LoopState,
    app: &mut App,
    snapshot: &FrontendSnapshot,
) -> (Vec<Effect>, Option<TuiOutcome>) {
    if matches!(snapshot.health, ServiceHealth::RuntimeEpochEnded { .. }) {
        state.active_operation_id = None;
        state.maintenance_id = None;
        state.ctrl_c.observe_idle();
        return (Vec::new(), None);
    }

    let waiting = matches!(
        state.ctrl_c,
        CtrlCPhase::CancelDispatching {
            operation_id: None,
            ..
        }
    );

    match &snapshot.availability {
        FrontendAvailability::Busy { operation_id } => {
            state.active_operation_id = Some(operation_id.clone());
            state.ctrl_c.observe_busy(operation_id.clone());
        }
        FrontendAvailability::Idle => {
            state.active_operation_id = None;
            state.ctrl_c.observe_idle();
        }
        FrontendAvailability::Compacting { .. } => {
            state.active_operation_id = None;
        }
    }

    state.maintenance_id = snapshot
        .maintenance
        .as_ref()
        .and_then(|maintenance| match maintenance.phase {
            FrontendMaintenancePhase::Accepted
            | FrontendMaintenancePhase::Started
            | FrontendMaintenancePhase::Progress => Some(maintenance.id.clone()),
            FrontendMaintenancePhase::Settled
            | FrontendMaintenancePhase::Failed
            | FrontendMaintenancePhase::Cancelled => None,
        });

    if waiting {
        if let Some(id) = state.ctrl_c.pending_cancel_id().map(str::to_owned) {
            let cancel_request = state
                .ctrl_c
                .cancel_request()
                .unwrap_or_else(|| state.next_cancel_request());
            return dispatch_cancel_operation(state, app, id, cancel_request);
        }
    }
    (Vec::new(), None)
}

fn apply_task(app: &mut App, completion: TaskCompletion) -> Vec<Effect> {
    match completion {
        TaskCompletion::Clipboard(result) => {
            let effects = finish_clipboard(app, result);
            app.ingest_appends(effects)
        }
        TaskCompletion::ClipboardWrite(result) => match result {
            Ok(()) => Vec::new(),
            Err(error) => app.ingest_appends(vec![Effect::Append(tasks::task_error(format!(
                "copy failed: {error}"
            )))]),
        },
        TaskCompletion::Media(MediaResult::Ready { .. }) => Vec::new(),
        TaskCompletion::Media(MediaResult::Refused {
            intent_id,
            kept,
            errors,
            ..
        }) => app.on_action(Action::SubmitMediaRefused {
            intent_id,
            kept,
            errors,
        }),
        TaskCompletion::Failed(error) => {
            app.ingest_appends(vec![Effect::Append(tasks::task_error(error))])
        }
    }
}

fn apply_input(
    app: &mut App,
    tasks: &mut PendingTasks,
    scheduler: &mut FrameScheduler,
    screen: TuiScreen,
    event_time: Instant,
    pending_zero_resize: &mut bool,
    event: TerminalInput,
) -> Vec<Effect> {
    match event {
        TerminalInput::Key(key) => {
            scheduler.invalidate_immediate(event_time);
            let action = keymap::interpret(&key);
            if matches!(action, crate::app::action::Action::Escape) {
                tasks.cancel_transient();
            }
            app.on_action(action)
        }
        TerminalInput::Paste(text) => {
            scheduler.invalidate_immediate(event_time);
            app.on_paste(&text)
        }
        TerminalInput::Mouse(mouse) => {
            let action = keymap::interpret_mouse(&mouse, app.is_selecting());
            if matches!(action, crate::app::action::Action::None) {
                Vec::new()
            } else {
                scheduler.invalidate_immediate(event_time);
                app.on_action(action)
            }
        }
        TerminalInput::Resize { .. } => {
            *pending_zero_resize = false;
            if screen == TuiScreen::Alternate {
                scheduler.request_hard_redraw(event_time);
            } else {
                scheduler.invalidate_immediate(event_time);
            }
            Vec::new()
        }
    }
}

enum InputFaultAction {
    Continue,
    ScheduleRebuild(Instant),
}

fn handle_input_fault(
    fault: TerminalInputFault,
    tracker: &mut InputFaultTracker,
    pending_zero_resize: &mut bool,
    now: Instant,
) -> Result<InputFaultAction, TuiOutcome> {
    match fault {
        TerminalInputFault::Interrupted | TerminalInputFault::WouldBlock => {
            tracker.ok();
            Ok(InputFaultAction::Continue)
        }
        TerminalInputFault::ZeroSizeResize => {
            tracker.ok();
            *pending_zero_resize = true;
            Ok(InputFaultAction::Continue)
        }
        TerminalInputFault::InvalidHandle => {
            if tracker.fail() || tracker.rebuilds_exhausted() {
                return Err(TuiOutcome::FallbackRequested {
                    fault: "terminal input handle rebuild budget exceeded".to_owned(),
                });
            }
            Ok(InputFaultAction::ScheduleRebuild(
                now + tracker.rebuild_backoff(),
            ))
        }
        TerminalInputFault::StreamTerminated => Err(TuiOutcome::FrontendRestartRequested {
            fault: "terminal input stream terminated".to_owned(),
        }),
        TerminalInputFault::ErrorBudgetExceeded { message } => {
            Err(TuiOutcome::FallbackRequested { fault: message })
        }
    }
}

fn dispatch_host(state: &mut LoopState, app: &mut App, request: HostRequest) -> Option<TuiOutcome> {
    let command = match request {
        HostRequest::NewSession => FrontendCommand::CreateSession,
        HostRequest::OpenSessions => FrontendCommand::ListSessions,
        HostRequest::LoadPreview(session_id) => {
            state.preview_generation = state.preview_generation.saturating_add(1);
            state.sync.preview_session_id = Some(session_id.clone());
            FrontendCommand::PreviewSession {
                session_id,
                request_generation: state.preview_generation,
            }
        }
        HostRequest::SwitchSession(session_id) => FrontendCommand::LoadSession { session_id },
        HostRequest::RebuildModel(name) => FrontendCommand::InstallModel { name },
        HostRequest::SetReasoning(effort) => FrontendCommand::SetReasoning { effort },
        HostRequest::ShowConfig => FrontendCommand::ReadConfig,
        HostRequest::ShowStatus => FrontendCommand::ReadStatus,
        HostRequest::Respond(confirmation_id, decision) => FrontendCommand::RespondConfirmation {
            confirmation_id,
            decision,
        },
    };
    match state.send(command.clone()) {
        CommandDispatch::Enqueued(id) => {
            match &command {
                FrontendCommand::PreviewSession { .. } => state.sync.preview_request = Some(id),
                FrontendCommand::InstallModel { .. } | FrontendCommand::SetReasoning { .. } => {
                    state.sync.model_request = Some(id);
                }
                _ => {}
            }
            None
        }
        CommandDispatch::Backpressured => {
            app.ingest_appends(vec![Effect::Append(tasks::task_error(
                "frontend command backpressured",
            ))]);
            None
        }
        CommandDispatch::Disconnected { .. } => Some(TuiOutcome::FrontendRestartRequested {
            fault: "frontend disconnected".to_owned(),
        }),
    }
}

fn assert_single_round_writes(report: FlushReport) {
    debug_assert!(report.clears <= 1);
    debug_assert!(report.inserts <= 1);
    debug_assert!(report.draws <= 1);
    debug_assert!(report.inserts == 0 || report.draws == 1);
}

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

/// After media decode succeeds, enqueue Submit and feed the structured result
/// back into the reducer. Local draft commits only on later `SubmitAccepted`.
fn submit_ready_with_outcome(
    state: &mut LoopState,
    app: &mut App,
    intent_id: u64,
    draft: String,
    attachments: Vec<philo_agent_service::FrontendAttachment>,
) -> Result<Vec<Effect>, TuiOutcome> {
    let dispatch = state.send(FrontendCommand::Submit { draft, attachments });
    match dispatch {
        CommandDispatch::Enqueued(request_id) => {
            state.remember_submit(request_id, intent_id);
            Ok(app.on_action(Action::SubmitDispatchFinished {
                intent_id,
                result: SubmitDispatchResult::Enqueued(request_id),
            }))
        }
        CommandDispatch::Backpressured => Ok(app.on_action(Action::SubmitDispatchFinished {
            intent_id,
            result: SubmitDispatchResult::Backpressured,
        })),
        CommandDispatch::Disconnected { lane } => {
            let _ = app.on_action(Action::SubmitDispatchFinished {
                intent_id,
                result: SubmitDispatchResult::Disconnected { lane },
            });
            Err(TuiOutcome::FrontendRestartRequested {
                fault: format!("frontend disconnected on submit ({lane})"),
            })
        }
    }
}

// Wire media-ready into the task path by submitting here.
fn apply_task_with_submit(
    app: &mut App,
    state: &mut LoopState,
    completion: TaskCompletion,
) -> Result<Vec<Effect>, TuiOutcome> {
    match completion {
        TaskCompletion::Media(MediaResult::Ready {
            intent_id,
            draft,
            attachments,
        }) => submit_ready_with_outcome(state, app, intent_id, draft, attachments),
        other => Ok(apply_task(app, other)),
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::platform::input::{FakeInputSource, TerminalInput, TerminalInputFault};

    use super::*;

    fn launch_config() -> TuiLaunchConfig {
        TuiLaunchConfig {
            session_id: "s-1".to_owned(),
            model_name: "base".to_owned(),
            verbose: false,
            show_reasoning: true,
            context_window: None,
            screen: TuiScreen::Inline,
            interrupt: None,
        }
    }

    fn ctrl_d() -> TerminalInput {
        TerminalInput::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
    }

    #[tokio::test]
    async fn interrupted_and_zero_resize_do_not_exit() {
        let (_service, client, _runtime) = philo_agent_service::testing::start_test_service();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("backend");
        let mut input = FakeInputSource::new([
            Err(TerminalInputFault::Interrupted),
            Err(TerminalInputFault::WouldBlock),
            Err(TerminalInputFault::ZeroSizeResize),
            Ok(TerminalInput::Resize {
                width: 80,
                height: 24,
            }),
            Ok(ctrl_d()),
        ]);
        let outcome = run_loop(
            client,
            launch_config(),
            &mut terminal,
            &mut input,
            false,
            TuiScreen::Inline,
        )
        .await;
        assert_eq!(outcome, TuiOutcome::UserExit);
    }

    #[tokio::test]
    async fn stream_terminated_requests_frontend_restart() {
        let (_service, client, _runtime) = philo_agent_service::testing::start_test_service();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("backend");
        let mut input = FakeInputSource::new([Err(TerminalInputFault::StreamTerminated)]);
        let outcome = run_loop(
            client,
            launch_config(),
            &mut terminal,
            &mut input,
            false,
            TuiScreen::Inline,
        )
        .await;
        assert!(matches!(
            outcome,
            TuiOutcome::FrontendRestartRequested { .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn input_rebuild_backoff_consumes_frontend_updates() {
        let (service, client, _runtime) = philo_agent_service::testing::start_test_service();
        let injector = client.clone();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("backend");
        let notified = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut input =
            FakeInputSource::new([Err(TerminalInputFault::InvalidHandle), Ok(ctrl_d())]);
        input.notify_on_invalid_handle(notified.clone());

        let inject = async move {
            notified.notified().await;
            let _dispatch = injector.try_command(philo_agent_service::FrontendCommand::ReadStatus);
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            tokio::time::advance(std::time::Duration::from_millis(10)).await;
        };
        tokio::spawn(inject);

        let outcome = run_loop(
            client,
            launch_config(),
            &mut terminal,
            &mut input,
            false,
            TuiScreen::Inline,
        )
        .await;
        assert_eq!(outcome, TuiOutcome::UserExit);
        assert_eq!(input.rebuilds(), 1);
        drop(service);
    }

    fn ctrl_c_key() -> TerminalInput {
        TerminalInput::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
    }

    fn busy_events(runtime: &philo_agent_service::testing::FakeRuntimeHandle, operation_id: &str) {
        runtime.emit(philo_agent_service::RuntimeEvent::OperationAccepted {
            operation_id: philo_agent_runtime::OperationId::new(operation_id),
            session_id: philo_agent_runtime::SessionId::new("s-1"),
            turn_id: philo_agent_runtime::TurnId::new("turn-1"),
        });
        runtime.emit(philo_agent_service::RuntimeEvent::AvailabilityChanged {
            availability: philo_agent_runtime::AgentAvailability::Busy {
                operation_id: philo_agent_runtime::OperationId::new(operation_id),
            },
            queued: 0,
        });
    }

    #[tokio::test(start_paused = true)]
    async fn input_rebuild_backoff_consumes_forced_signal() {
        let (_service, client, _runtime) = philo_agent_service::testing::start_test_service();
        let (tx, rx) = tokio::sync::watch::channel(0u64);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("backend");
        let notified = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut input = FakeInputSource::new([Err(TerminalInputFault::InvalidHandle)]);
        input.notify_on_invalid_handle(notified.clone());
        let mut config = launch_config();
        config.interrupt = Some(rx);

        let inject = async move {
            notified.notified().await;
            tx.send_modify(|n| *n = n.saturating_add(1));
        };
        tokio::spawn(inject);

        let outcome = run_loop(
            client,
            config,
            &mut terminal,
            &mut input,
            false,
            TuiScreen::Inline,
        )
        .await;
        assert_eq!(outcome, TuiOutcome::UserExit);
    }

    #[tokio::test]
    async fn busy_second_interrupt_requests_forced_exit() {
        let (service, client, runtime) = philo_agent_service::testing::start_test_service();
        let (tx, rx) = tokio::sync::watch::channel(0u64);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("backend");
        let notified = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut input = FakeInputSource::new([Err(TerminalInputFault::InvalidHandle)]);
        input.notify_on_invalid_handle(notified.clone());
        let mut config = launch_config();
        config.interrupt = Some(rx);

        let inject = async move {
            notified.notified().await;
            busy_events(&runtime, "op-1");
            for _ in 0..32 {
                tokio::task::yield_now().await;
            }
            tx.send_modify(|n| *n = n.saturating_add(1));
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            tx.send_modify(|n| *n = n.saturating_add(1));
        };
        tokio::spawn(inject);

        let outcome = run_loop(
            client,
            config,
            &mut terminal,
            &mut input,
            false,
            TuiScreen::Inline,
        )
        .await;
        assert_eq!(outcome, TuiOutcome::ForcedExitRequested { code: 130 });
        drop(service);
    }

    #[tokio::test]
    async fn settled_operation_resets_interrupt_to_first_press() {
        let (service, client, runtime) = philo_agent_service::testing::start_test_service();
        let (tx, rx) = tokio::sync::watch::channel(0u64);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("backend");
        let notified = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut input = FakeInputSource::new([Err(TerminalInputFault::InvalidHandle)]);
        input.notify_on_invalid_handle(notified.clone());
        let mut config = launch_config();
        config.interrupt = Some(rx);

        let inject = async move {
            notified.notified().await;
            busy_events(&runtime, "op-1");
            for _ in 0..32 {
                tokio::task::yield_now().await;
            }
            tx.send_modify(|n| *n = n.saturating_add(1));
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            runtime.emit(philo_agent_service::RuntimeEvent::AvailabilityChanged {
                availability: philo_agent_runtime::AgentAvailability::Idle,
                queued: 0,
            });
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            tx.send_modify(|n| *n = n.saturating_add(1));
        };
        tokio::spawn(inject);

        let outcome = run_loop(
            client,
            config,
            &mut terminal,
            &mut input,
            false,
            TuiScreen::Inline,
        )
        .await;
        assert_eq!(outcome, TuiOutcome::UserExit);
        drop(service);
    }

    #[tokio::test(start_paused = true)]
    async fn busy_second_keyboard_ctrl_c_requests_forced_exit() {
        let (service, client, runtime) = philo_agent_service::testing::start_test_service();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("backend");
        let notified = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut input = FakeInputSource::new([
            Err(TerminalInputFault::InvalidHandle),
            Ok(ctrl_c_key()),
            Ok(ctrl_c_key()),
        ]);
        input.notify_on_invalid_handle(notified.clone());

        let inject = async move {
            notified.notified().await;
            busy_events(&runtime, "op-1");
            for _ in 0..32 {
                tokio::task::yield_now().await;
            }
            tokio::time::advance(std::time::Duration::from_millis(10)).await;
        };
        tokio::spawn(inject);

        let outcome = run_loop(
            client,
            launch_config(),
            &mut terminal,
            &mut input,
            false,
            TuiScreen::Inline,
        )
        .await;
        assert_eq!(outcome, TuiOutcome::ForcedExitRequested { code: 130 });
        drop(service);
    }

    #[tokio::test]
    async fn unmatched_submit_accepted_does_not_commit_current_intent() {
        let (_service, client, _runtime) = philo_agent_service::testing::start_test_service();
        let mut app = App::new(StatusData::new("m", "s", InfoLevel::Default), true);
        for ch in "draft".chars() {
            app.on_action(Action::InsertChar(ch));
        }
        let effects = app.on_action(Action::Submit);
        let intent_id = match effects.as_slice() {
            [Effect::PrepareSubmit { intent_id, .. }] => *intent_id,
            other => panic!("expected PrepareSubmit, got {other:?}"),
        };
        let request_id = philo_agent_service::FrontendRequestId::new(1);
        app.on_action(Action::SubmitDispatchFinished {
            intent_id,
            result: SubmitDispatchResult::Enqueued(request_id),
        });

        let mut state = loop_state(client);
        state.remember_submit(request_id, intent_id);

        let unmatched = FrontendUpdate {
            epoch: philo_agent_service::FrontendEpoch::INITIAL,
            revision: FrontendRevision::new(1),
            request_id: Some(philo_agent_service::FrontendRequestId::new(99)),
            kind: FrontendUpdateKind::SubmitAccepted {
                operation_id: "op-stale".to_owned(),
                turn_id: "turn-stale".to_owned(),
            },
        };
        let effects = apply_updates(
            &mut app,
            &mut MarkdownRenderer::new(),
            &mut state,
            vec![unmatched],
        )
        .expect("apply");
        assert!(effects.is_empty());
        assert!(matches!(
            app.submit_state(),
            crate::app::submit::SubmitState::Dispatching(_)
        ));
        assert!(
            app.cells
                .cells()
                .iter()
                .all(|line| line.kind != LineKind::User)
        );
    }

    fn loop_state(client: FrontendClient) -> LoopState {
        LoopState {
            client,
            sync: FrontendSync::new(),
            preview_generation: 0,
            active_operation_id: None,
            maintenance_id: None,
            ctrl_c: CtrlCPhase::Idle,
            next_cancel_request: 1,
            submit_requests: Vec::new(),
            pending_session_load: None,
            session_load_retries: 0,
        }
    }

    fn dispatching_app(draft: &str) -> (App, u64, philo_agent_service::FrontendRequestId) {
        let mut app = App::new(StatusData::new("m", "s", InfoLevel::Default), true);
        for ch in draft.chars() {
            app.on_action(Action::InsertChar(ch));
        }
        let effects = app.on_action(Action::Submit);
        let intent_id = match effects.as_slice() {
            [Effect::PrepareSubmit { intent_id, .. }] => *intent_id,
            other => panic!("expected PrepareSubmit, got {other:?}"),
        };
        let request_id = philo_agent_service::FrontendRequestId::new(1);
        app.on_action(Action::SubmitDispatchFinished {
            intent_id,
            result: SubmitDispatchResult::Enqueued(request_id),
        });
        (app, intent_id, request_id)
    }

    #[test]
    fn accept_allows_command_replies_while_awaiting_snapshot() {
        let mut sync = FrontendSync::new();
        sync.awaiting_snapshot = true;
        let accepted = FrontendUpdate {
            epoch: philo_agent_service::FrontendEpoch::INITIAL,
            revision: FrontendRevision::new(1),
            request_id: Some(philo_agent_service::FrontendRequestId::new(1)),
            kind: FrontendUpdateKind::SubmitAccepted {
                operation_id: "op-1".to_owned(),
                turn_id: "turn-1".to_owned(),
            },
        };
        assert!(sync.accept(&accepted));
        assert!(sync.awaiting_snapshot);

        let rejected = FrontendUpdate {
            revision: FrontendRevision::new(2),
            kind: FrontendUpdateKind::CommandRejected {
                reason: philo_agent_service::CommandReject::NoCurrentSession,
            },
            ..accepted.clone()
        };
        assert!(sync.accept(&rejected));

        let health = FrontendUpdate {
            revision: FrontendRevision::new(3),
            kind: FrontendUpdateKind::ServiceHealthChanged {
                health: philo_agent_service::ServiceHealth::Degraded {
                    message: "lag".to_owned(),
                },
            },
            ..accepted
        };
        assert!(sync.accept(&health));
        assert!(sync.awaiting_snapshot);
    }

    #[tokio::test]
    async fn resync_then_late_submit_accepted_leaves_dispatching() {
        let (_service, client, _runtime) = philo_agent_service::testing::start_test_service();
        let (mut app, intent_id, request_id) = dispatching_app("draft");
        let mut state = loop_state(client);
        state.remember_submit(request_id, intent_id);
        state.sync.awaiting_snapshot = true;

        let resync = FrontendUpdate {
            epoch: philo_agent_service::FrontendEpoch::INITIAL,
            revision: FrontendRevision::new(1),
            request_id: None,
            kind: FrontendUpdateKind::ResyncRequired {
                latest_revision: FrontendRevision::new(1),
            },
        };
        let accepted = FrontendUpdate {
            epoch: philo_agent_service::FrontendEpoch::INITIAL,
            revision: FrontendRevision::new(2),
            request_id: Some(request_id),
            kind: FrontendUpdateKind::SubmitAccepted {
                operation_id: "op-late".to_owned(),
                turn_id: "turn-late".to_owned(),
            },
        };
        apply_updates(
            &mut app,
            &mut MarkdownRenderer::new(),
            &mut state,
            vec![resync, accepted],
        )
        .expect("apply");
        assert!(matches!(
            app.submit_state(),
            crate::app::submit::SubmitState::Accepted { .. }
        ));
        assert!(
            app.cells
                .cells()
                .iter()
                .any(|line| line.kind == LineKind::User && line.text.contains("draft"))
        );
    }

    #[tokio::test]
    async fn snapshot_busy_ctrl_c_cancels_with_id() {
        let (_service, client, _runtime) = philo_agent_service::testing::start_test_service();
        let mut app = App::new(StatusData::new("m", "s", InfoLevel::Default), true);
        let mut state = loop_state(client);
        let update = FrontendUpdate {
            epoch: philo_agent_service::FrontendEpoch::INITIAL,
            revision: FrontendRevision::new(1),
            request_id: None,
            kind: FrontendUpdateKind::SnapshotReady(Box::new(
                crate::tests::support::busy_snapshot("s", "op-snap"),
            )),
        };
        apply_updates(
            &mut app,
            &mut MarkdownRenderer::new(),
            &mut state,
            vec![update],
        )
        .expect("apply");
        assert_eq!(state.active_operation_id.as_deref(), Some("op-snap"));
        let decision = ctrl_c_decision(&mut state, true);
        assert!(matches!(
            decision,
            CtrlCDecision::Cancel {
                operation_id,
                ..
            } if operation_id == "op-snap"
        ));
    }

    #[test]
    fn backpressured_snapshot_request_does_not_await() {
        let mut sync = FrontendSync::new();
        sync.want_snapshot = true;
        sync.snapshot_retries = SNAPSHOT_REQUEST_RETRY_BUDGET;
        let effects = apply_snapshot_request_dispatch(&mut sync, CommandDispatch::Backpressured)
            .expect("dispatch");
        assert!(!sync.awaiting_snapshot);
        assert!(sync.want_snapshot);
        assert_eq!(sync.snapshot_retries, SNAPSHOT_REQUEST_RETRY_BUDGET - 1);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Append(lines) if lines.iter().any(|line| line.text.contains("快照请求未发送"))))
        );

        sync.snapshot_retries = 0;
        let _ = apply_snapshot_request_dispatch(&mut sync, CommandDispatch::Backpressured)
            .expect("exhausted");
        assert!(!sync.awaiting_snapshot);
        assert!(!sync.want_snapshot);
    }

    #[tokio::test]
    async fn submit_accepted_matches_pending_request_id_without_map() {
        let (_service, client, _runtime) = philo_agent_service::testing::start_test_service();
        let (mut app, _intent_id, request_id) = dispatching_app("draft");
        let mut state = loop_state(client);
        state.sync.awaiting_snapshot = true;
        let accepted = FrontendUpdate {
            epoch: philo_agent_service::FrontendEpoch::INITIAL,
            revision: FrontendRevision::new(1),
            request_id: Some(request_id),
            kind: FrontendUpdateKind::SubmitAccepted {
                operation_id: "op-1".to_owned(),
                turn_id: "turn-1".to_owned(),
            },
        };
        apply_updates(
            &mut app,
            &mut MarkdownRenderer::new(),
            &mut state,
            vec![accepted],
        )
        .expect("apply");
        assert!(matches!(
            app.submit_state(),
            crate::app::submit::SubmitState::Accepted { .. }
        ));
    }
}
