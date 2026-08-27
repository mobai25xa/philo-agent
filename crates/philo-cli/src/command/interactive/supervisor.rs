//! ProcessSupervisor: owns the Tokio runtime, AgentService, frontend restart
//! budget, and shutdown. It does not assemble models, tools, or UI state.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use philo_agent_service::{
    AgentService, AttachError, CommandDispatch, DetachError, DetachReport, FRONTEND_RESTART_BUDGET,
    FRONTEND_RESTART_WINDOW_SECS, FrontendInstanceId, FrontendRevision,
};
use philo_tui::{
    RestoreFailure, RestoreReport, TerminalCapability, TuiLaunchConfig, TuiOutcome, TuiRecovery,
    TuiRecoveryAttachment, TuiRunReport, run_async,
};
use tokio::sync::watch;

use crate::assembly::{self, Bootstrap};
use crate::command::ctrl_c;
use crate::command::oneshot::drive;
use crate::config::{Verbosity, WatchTask};
use crate::error::UsageError;
use crate::render::{self, Channel, Output};

/// Sliding window of frontend faults. Three restarts in 60s exhausts the budget.
pub(crate) struct RestartBudget {
    stamps: VecDeque<Instant>,
    max: u32,
    window: Duration,
}

impl RestartBudget {
    pub(crate) fn new() -> Self {
        Self {
            stamps: VecDeque::new(),
            max: FRONTEND_RESTART_BUDGET,
            window: Duration::from_secs(FRONTEND_RESTART_WINDOW_SECS),
        }
    }

    /// Records one restart. Returns false when the budget is exhausted.
    pub(crate) fn allow(&mut self) -> bool {
        let now = Instant::now();
        self.stamps
            .retain(|stamp| now.duration_since(*stamp) < self.window);
        if self.stamps.len() >= self.max as usize {
            return false;
        }
        self.stamps.push_back(now);
        true
    }
}

#[derive(Debug)]
pub(super) enum RegisteredFrontend<T> {
    AttachFailed(AttachError),
    Finished {
        output: Result<T, ()>,
        detach: Result<DetachReport, DetachError>,
    },
}

/// What the process supervisor may do after a detach attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DetachFollowUp {
    /// Lease is gone (`Ok` or `StaleLease`). TUI outcome may restart.
    Continue,
    /// Send succeeded or the lane was full; actor may still hold the lease.
    /// Cleanup via supervisor shutdown; do not attach a new frontend id.
    UncertainCleanup,
    /// Host is already gone. Process shutdown, no TUI restart.
    ServiceUnavailable,
}

fn detach_follow_up(detach: &Result<DetachReport, DetachError>) -> DetachFollowUp {
    match detach {
        Ok(_) | Err(DetachError::StaleLease) => DetachFollowUp::Continue,
        Err(DetachError::DeadlineExceeded | DetachError::Backpressured) => {
            DetachFollowUp::UncertainCleanup
        }
        Err(DetachError::ServiceGone | DetachError::Disconnected) => {
            DetachFollowUp::ServiceUnavailable
        }
    }
}

fn run_registered_frontend<T>(
    runtime: &tokio::runtime::Runtime,
    service: &AgentService,
    frontend_id: FrontendInstanceId,
    deadline: Instant,
    runner: impl FnOnce() -> T,
) -> RegisteredFrontend<T> {
    let lease = match runtime.block_on(service.attach_frontend(frontend_id, deadline)) {
        Ok(lease) => lease,
        Err(error) => return RegisteredFrontend::AttachFailed(error),
    };
    let output = catch_unwind(AssertUnwindSafe(runner)).map_err(|_| ());
    let detach_deadline = Instant::now() + assembly::FRONTEND_REGISTRATION_GRACE;
    let detach = runtime.block_on(service.detach_frontend(lease, detach_deadline));
    RegisteredFrontend::Finished { output, detach }
}

pub(super) struct ProcessSupervisor {
    runtime: tokio::runtime::Runtime,
    bootstrap: Bootstrap,
    _watch: WatchTask,
    budget: RestartBudget,
    instance: u64,
    interrupt_rx: watch::Receiver<u64>,
}

impl ProcessSupervisor {
    pub(super) fn new(
        runtime: tokio::runtime::Runtime,
        bootstrap: Bootstrap,
        watch: WatchTask,
    ) -> Self {
        let (interrupt_tx, interrupt_rx) = watch::channel(0u64);
        runtime.spawn(ctrl_c::forward_os_ctrl_c(interrupt_tx));
        Self {
            runtime,
            bootstrap,
            _watch: watch,
            budget: RestartBudget::new(),
            instance: 0,
            interrupt_rx,
        }
    }

    pub(super) fn run(mut self, session_id: String) -> Result<ExitCode, UsageError> {
        let mut pending_recovery = None;
        loop {
            self.instance += 1;
            let instance_id = FrontendInstanceId::new(format!("cli-tui-{}", self.instance));
            if self.instance > 1 {
                match self
                    .bootstrap
                    .client
                    .request_snapshot(FrontendRevision::ZERO)
                {
                    CommandDispatch::Enqueued(_)
                    | CommandDispatch::Backpressured
                    | CommandDispatch::Disconnected { .. } => {}
                }
            }

            let client = self.bootstrap.client.clone();
            let session = run_registered_frontend(
                &self.runtime,
                &self.bootstrap.service,
                instance_id,
                Instant::now() + assembly::FRONTEND_REGISTRATION_GRACE,
                || {
                    let config = launch_config(
                        &self.bootstrap,
                        session_id.clone(),
                        self.interrupt_rx.clone(),
                        pending_recovery.take(),
                    );
                    self.runtime.block_on(run_async(client, config))
                },
            );

            let report = match session {
                RegisteredFrontend::AttachFailed(error) => {
                    eprintln!("error: frontend attach failed: {error}");
                    report_recovery(pending_recovery.as_ref());
                    return Ok(self.shutdown(1, Instant::now() + assembly::PROCESS_SHUTDOWN_GRACE));
                }
                RegisteredFrontend::Finished { output, detach } => {
                    match detach_follow_up(&detach) {
                        DetachFollowUp::UncertainCleanup => {
                            if let Err(error) = &detach {
                                eprintln!("error: frontend detach: {error}");
                            }
                            report_recovery(
                                output
                                    .as_ref()
                                    .ok()
                                    .and_then(|report| report.recovery.as_ref()),
                            );
                            return Ok(self.cleanup_uncertain_detach());
                        }
                        DetachFollowUp::ServiceUnavailable => {
                            if let Err(error) = &detach {
                                eprintln!("error: frontend detach: {error}");
                            }
                            report_recovery(
                                output
                                    .as_ref()
                                    .ok()
                                    .and_then(|report| report.recovery.as_ref()),
                            );
                            return Ok(
                                self.shutdown(1, Instant::now() + assembly::PROCESS_SHUTDOWN_GRACE)
                            );
                        }
                        DetachFollowUp::Continue => {
                            if let Err(error) = detach {
                                eprintln!("error: frontend detach: {error}");
                            }
                            match output {
                                Ok(report) => report,
                                Err(()) => TuiRunReport {
                                    outcome: TuiOutcome::FrontendRestartRequested {
                                        fault: "frontend panicked".to_owned(),
                                    },
                                    restore: uncertain_panic_restore(),
                                    recovery: None,
                                },
                            }
                        }
                    }
                }
            };

            let TuiRunReport {
                outcome,
                restore,
                recovery,
            } = report;
            report_restore(&restore);
            pending_recovery = recovery;
            match outcome {
                TuiOutcome::UserExit | TuiOutcome::ProcessShutdownRequested => {
                    return Ok(self.shutdown(0, Instant::now() + assembly::PROCESS_SHUTDOWN_GRACE));
                }
                TuiOutcome::FrontendRestartRequested { fault: _ } => {
                    if !self.budget.allow() {
                        return Ok(self.fallback(
                            "frontend restart budget exhausted",
                            session_id,
                            pending_recovery.take(),
                        ));
                    }
                }
                TuiOutcome::FallbackRequested { fault } => {
                    return Ok(self.fallback(&fault, session_id, pending_recovery.take()));
                }
                TuiOutcome::ForcedExitRequested { code } => {
                    report_forced_exit();
                    return Ok(self.shutdown(code, Instant::now()));
                }
            }
        }
    }

    fn fallback(self, fault: &str, session_id: String, recovery: Option<TuiRecovery>) -> ExitCode {
        eprintln!("error: falling back to line output: {fault}");
        report_recovery(recovery.as_ref());
        let client = self.bootstrap.client.clone();
        let sessions = self.bootstrap.sessions.clone();
        let verbosity = self.bootstrap.settings.verbosity;
        let show_reasoning = self.bootstrap.settings.show_reasoning;
        let interrupt = self.interrupt_rx.clone();
        let report = self.runtime.block_on(drive::run(drive::Request {
            client,
            sessions: Some(sessions),
            session_id,
            continues_existing: false,
            user_message: None,
            verbosity,
            show_reasoning,
            success_exit: 1,
            interrupt,
        }));
        if report.forced {
            self.shutdown(report.code, Instant::now())
        } else {
            self.shutdown(
                report.code,
                Instant::now() + assembly::PROCESS_SHUTDOWN_GRACE,
            )
        }
    }

    fn cleanup_uncertain_detach(self) -> ExitCode {
        let deadline = Instant::now() + assembly::PROCESS_SHUTDOWN_GRACE;
        if let Err(error) = self.runtime.block_on(
            self.bootstrap
                .service
                .shutdown_from_supervisor("uncertain frontend detach", deadline),
        ) {
            eprintln!("error: supervisor cleanup after uncertain detach: {error}");
        }
        self.shutdown(1, Instant::now() + assembly::PROCESS_SHUTDOWN_GRACE)
    }

    fn shutdown(self, code: u8, deadline: Instant) -> ExitCode {
        let Self {
            runtime,
            bootstrap,
            _watch,
            mut interrupt_rx,
            ..
        } = self;
        let pending = runtime.block_on(async {
            let mut report =
                assembly::shutdown_with_deadline(bootstrap, &mut interrupt_rx, deadline).await;
            assembly::join_watch_with_deadline(_watch, deadline, &mut report).await;
            for name in &report.pending {
                eprintln!("error: shutdown deadline exceeded: {name}");
            }
            for diagnostic in &report.diagnostics {
                eprintln!("error: {diagnostic}");
            }
            report.pending
        });
        ExitCode::from(assembly::shutdown_exit_code(code, &pending))
    }
}

fn report_restore(restore: &RestoreReport) {
    render::write_outputs(&restore_line_outputs(restore));
}

fn report_recovery(recovery: Option<&TuiRecovery>) {
    render::write_outputs(&recovery_line_outputs(recovery));
}

fn recovery_line_outputs(recovery: Option<&TuiRecovery>) -> Vec<Output> {
    let Some(recovery) = recovery else {
        return Vec::new();
    };
    let mut outputs = Vec::new();
    if !recovery.draft.is_empty() {
        outputs.push(Output {
            channel: Channel::Stderr,
            text: format!("unsent draft preserved:\n{}\n", recovery.draft),
        });
    }
    outputs.extend(recovery.attachments.iter().map(|attachment| {
        let label = match attachment {
            TuiRecoveryAttachment::Path(path) => path.clone(),
            TuiRecoveryAttachment::Image {
                media_type,
                bytes,
                origin,
            } => format!("{origin} ({media_type}, {} bytes)", bytes.len()),
        };
        Output {
            channel: Channel::Stderr,
            text: format!("unsent attachment preserved: {label}\n"),
        }
    }));
    outputs
}

fn uncertain_panic_restore() -> RestoreReport {
    RestoreReport {
        restored: false,
        skipped_stale: false,
        attempted: Vec::new(),
        restored_caps: Vec::new(),
        failures: vec![RestoreFailure {
            capability: TerminalCapability::RawMode,
            message: "uncertain ownership: frontend panicked before restore report".to_owned(),
        }],
    }
}

fn report_forced_exit() {
    render::write_outputs(&[forced_exit_notice()]);
}

fn restore_line_outputs(restore: &RestoreReport) -> Vec<Output> {
    restore
        .failures
        .iter()
        .map(|failure| Output {
            channel: Channel::Stderr,
            text: format!("terminal restore: {}\n", failure.message),
        })
        .collect()
}

fn forced_exit_notice() -> Output {
    Output {
        channel: Channel::Stderr,
        text: "forced exit: the session state may be unconfirmed\n".to_owned(),
    }
}

fn launch_config(
    bootstrap: &Bootstrap,
    session_id: String,
    interrupt: watch::Receiver<u64>,
    recovery: Option<TuiRecovery>,
) -> TuiLaunchConfig {
    TuiLaunchConfig {
        session_id,
        model_name: bootstrap.settings.deployment.model.clone(),
        verbose: bootstrap.settings.verbosity == Verbosity::Verbose,
        show_reasoning: bootstrap.settings.show_reasoning,
        context_window: bootstrap.settings.context_window,
        interrupt: Some(interrupt),
        workspace_root: bootstrap.workspace_root.clone(),
        recovery,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use philo_agent_service::testing::{abort_service_actor_and_wait, start_test_service};

    #[test]
    fn restart_budget_allows_three_faults_then_denies() {
        let mut budget = RestartBudget::new();
        assert!(budget.allow());
        assert!(budget.allow());
        assert!(budget.allow());
        assert!(!budget.allow());
    }

    #[test]
    fn fallback_makes_unsent_recovery_visible() {
        let recovery = TuiRecovery {
            draft: "keep this".to_owned(),
            attachments: vec![
                TuiRecoveryAttachment::Path("shots/a.png".to_owned()),
                TuiRecoveryAttachment::Image {
                    media_type: "image/png".to_owned(),
                    bytes: vec![1, 2, 3],
                    origin: "clipboard image".to_owned(),
                },
            ],
        };

        let outputs = recovery_line_outputs(Some(&recovery));
        assert_eq!(outputs.len(), 3);
        assert!(outputs[0].text.contains("keep this"));
        assert!(outputs[1].text.contains("shots/a.png"));
        assert!(
            outputs[2]
                .text
                .contains("clipboard image (image/png, 3 bytes)")
        );
        assert!(
            outputs
                .iter()
                .all(|output| output.channel == Channel::Stderr)
        );
    }

    #[test]
    fn restore_errors_precede_forced_exit_notice() {
        let restore = RestoreReport {
            restored: true,
            skipped_stale: false,
            attempted: Vec::new(),
            restored_caps: Vec::new(),
            failures: vec![
                philo_tui::RestoreFailure {
                    capability: philo_tui::TerminalCapability::RawMode,
                    message: "raw mode failed".to_owned(),
                },
                philo_tui::RestoreFailure {
                    capability: philo_tui::TerminalCapability::MouseCapture,
                    message: "mouse capture".to_owned(),
                },
            ],
        };
        let mut outputs = restore_line_outputs(&restore);
        outputs.push(forced_exit_notice());
        assert_eq!(outputs[0].text, "terminal restore: raw mode failed\n");
        assert_eq!(outputs[1].text, "terminal restore: mouse capture\n");
        assert_eq!(
            outputs[2].text,
            "forced exit: the session state may be unconfirmed\n"
        );
        assert!(outputs.iter().all(|o| o.channel == Channel::Stderr));
        assert_eq!(ctrl_c::FORCED_EXIT_CODE, 130);
    }

    #[test]
    fn forced_exit_outcome_still_reports_restore_first() {
        assert!(matches!(
            TuiOutcome::ForcedExitRequested { code: 130 },
            TuiOutcome::ForcedExitRequested { .. }
        ));
        let restore = RestoreReport {
            restored: true,
            skipped_stale: false,
            attempted: Vec::new(),
            restored_caps: Vec::new(),
            failures: vec![philo_tui::RestoreFailure {
                capability: philo_tui::TerminalCapability::AlternateScreen,
                message: "leave alternate".to_owned(),
            }],
        };
        let restore_first = restore_line_outputs(&restore);
        assert_eq!(restore_first[0].text, "terminal restore: leave alternate\n");
        let notice = forced_exit_notice();
        assert!(
            restore_first[0].text.starts_with("terminal restore:"),
            "restore diagnostics must be produced before the exit notice"
        );
        assert!(notice.text.contains("unconfirmed"));
    }

    #[test]
    fn pulse_sender_is_not_the_exit_path() {
        let (tx, mut rx) = watch::channel(0u64);
        let mut seen = ctrl_c::skip_past_pulses(&mut rx);
        ctrl_c::pulse(&tx);
        ctrl_c::pulse(&tx);
        assert_eq!(ctrl_c::take_pulses(&mut rx, &mut seen), 2);
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    #[test]
    fn attach_ack_precedes_runner() {
        let runtime = test_runtime();
        let _enter = runtime.enter();
        let (service, _client, _handle) = start_test_service();
        let mut started = false;
        let session = run_registered_frontend(
            &runtime,
            &service,
            FrontendInstanceId::new("cli-tui-1"),
            Instant::now() + Duration::from_secs(2),
            || {
                started = true;
                "ran"
            },
        );
        assert!(started);
        assert!(matches!(
            session,
            RegisteredFrontend::Finished {
                output: Ok("ran"),
                detach: Ok(_),
            }
        ));
    }

    #[test]
    fn attach_failure_does_not_start_runner() {
        let runtime = test_runtime();
        let _enter = runtime.enter();
        let (service, _client, _handle) = start_test_service();
        runtime.block_on(abort_service_actor_and_wait(&service));
        let mut started = false;
        let session = run_registered_frontend(
            &runtime,
            &service,
            FrontendInstanceId::new("cli-tui-1"),
            Instant::now() + Duration::from_millis(200),
            || {
                started = true;
                "ran"
            },
        );
        assert!(!started);
        assert!(matches!(session, RegisteredFrontend::AttachFailed(_)));
    }

    #[test]
    fn detach_runs_after_normal_error_and_panic() {
        let runtime = test_runtime();
        let _enter = runtime.enter();
        let (service, _client, _handle) = start_test_service();

        let error_session = run_registered_frontend(
            &runtime,
            &service,
            FrontendInstanceId::new("cli-tui-err"),
            Instant::now() + Duration::from_secs(2),
            || Err::<(), &str>("tui error"),
        );
        assert!(matches!(
            error_session,
            RegisteredFrontend::Finished {
                output: Ok(Err("tui error")),
                detach: Ok(_),
            }
        ));

        let panic_session = run_registered_frontend(
            &runtime,
            &service,
            FrontendInstanceId::new("cli-tui-panic"),
            Instant::now() + Duration::from_secs(2),
            || panic!("frontend panicked"),
        );
        assert!(matches!(
            panic_session,
            RegisteredFrontend::Finished {
                output: Err(()),
                detach: Ok(_),
            }
        ));
    }

    #[test]
    fn panic_restore_is_marked_uncertain() {
        let restore = uncertain_panic_restore();
        assert!(!restore.failures.is_empty());
        let outputs = restore_line_outputs(&restore);
        assert!(
            outputs
                .iter()
                .any(|line| line.text.contains("uncertain") && line.channel == Channel::Stderr),
            "{outputs:?}"
        );
    }

    #[test]
    fn runner_exceeding_attach_grace_still_detaches() {
        let runtime = test_runtime();
        let _enter = runtime.enter();
        let (service, _client, _handle) = start_test_service();
        let session = run_registered_frontend(
            &runtime,
            &service,
            FrontendInstanceId::new("cli-tui-long"),
            Instant::now() + Duration::from_millis(50),
            || {
                std::thread::sleep(Duration::from_millis(80));
                "ran"
            },
        );
        assert!(matches!(
            session,
            RegisteredFrontend::Finished {
                output: Ok("ran"),
                detach: Ok(_),
            }
        ));
    }

    #[test]
    fn only_confirmed_or_stale_detach_allows_restart() {
        assert_eq!(
            detach_follow_up(&Err(DetachError::StaleLease)),
            DetachFollowUp::Continue
        );
        assert_eq!(
            detach_follow_up(&Err(DetachError::DeadlineExceeded)),
            DetachFollowUp::UncertainCleanup
        );
        assert_eq!(
            detach_follow_up(&Err(DetachError::Backpressured)),
            DetachFollowUp::UncertainCleanup
        );
        assert_eq!(
            detach_follow_up(&Err(DetachError::ServiceGone)),
            DetachFollowUp::ServiceUnavailable
        );
        assert_eq!(
            detach_follow_up(&Err(DetachError::Disconnected)),
            DetachFollowUp::ServiceUnavailable
        );
    }

    #[test]
    fn uncertain_detach_must_not_attach_a_new_frontend_id() {
        let runtime = test_runtime();
        let _enter = runtime.enter();
        let (service, _client, _handle) = start_test_service();
        runtime
            .block_on(service.attach_frontend(
                FrontendInstanceId::new("cli-tui-1"),
                Instant::now() + Duration::from_secs(2),
            ))
            .expect("attach");

        // Caller saw DeadlineExceeded after send; actor still holds the lease.
        let detach: Result<DetachReport, DetachError> = Err(DetachError::DeadlineExceeded);
        assert_eq!(detach_follow_up(&detach), DetachFollowUp::UncertainCleanup);

        let occupied = runtime.block_on(service.attach_frontend(
            FrontendInstanceId::new("cli-tui-2"),
            Instant::now() + Duration::from_secs(2),
        ));
        assert!(
            matches!(occupied, Err(AttachError::FrontendOccupied { .. })),
            "old restart path would Occupied-kill here: {occupied:?}"
        );

        runtime
            .block_on(service.shutdown_from_supervisor(
                "uncertain frontend detach",
                Instant::now() + Duration::from_secs(2),
            ))
            .expect("supervisor cleanup");
        let after = runtime.block_on(service.attach_frontend(
            FrontendInstanceId::new("cli-tui-2"),
            Instant::now() + Duration::from_secs(2),
        ));
        assert!(
            matches!(
                after,
                Err(AttachError::Disconnected) | Err(AttachError::ServiceGone)
            ),
            "cleanup must not leave Occupied: {after:?}"
        );
        assert!(!matches!(after, Err(AttachError::FrontendOccupied { .. })));
    }

    #[test]
    fn confirmed_detach_allows_a_new_frontend_id() {
        let runtime = test_runtime();
        let _enter = runtime.enter();
        let (service, _client, _handle) = start_test_service();
        let first = run_registered_frontend(
            &runtime,
            &service,
            FrontendInstanceId::new("cli-tui-1"),
            Instant::now() + Duration::from_secs(2),
            || "first",
        );
        let detach = match first {
            RegisteredFrontend::Finished {
                detach: Ok(report),
                output: Ok("first"),
            } => Ok(report),
            other => panic!("expected confirmed detach: {other:?}"),
        };
        assert_eq!(detach_follow_up(&detach), DetachFollowUp::Continue);

        let second = run_registered_frontend(
            &runtime,
            &service,
            FrontendInstanceId::new("cli-tui-2"),
            Instant::now() + Duration::from_secs(2),
            || "second",
        );
        assert!(matches!(
            second,
            RegisteredFrontend::Finished {
                output: Ok("second"),
                detach: Ok(_),
            }
        ));
    }
}
