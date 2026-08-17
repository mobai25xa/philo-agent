//! ProcessSupervisor: owns the Tokio runtime, AgentService, frontend restart
//! budget, and shutdown. It does not assemble models, tools, or UI state.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use philo_agent_service::{
    DetachReason, FRONTEND_RESTART_BUDGET, FRONTEND_RESTART_WINDOW_SECS, FrontendCommand,
    FrontendInstanceId, FrontendRevision,
};
use philo_tui::{RestoreReport, TuiLaunchConfig, TuiOutcome, TuiRunReport, run_async};
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
        let mut last_instance: Option<FrontendInstanceId> = None;
        loop {
            self.instance += 1;
            let instance_id = FrontendInstanceId::new(format!("cli-tui-{}", self.instance));
            if let Some(previous) = last_instance.take() {
                let _ = self
                    .bootstrap
                    .client
                    .try_command(FrontendCommand::FrontendDetached {
                        frontend_instance_id: previous,
                        reason: DetachReason::Restart,
                    });
            }
            let _ = self
                .bootstrap
                .client
                .try_command(FrontendCommand::FrontendAttached {
                    frontend_instance_id: instance_id.clone(),
                });
            if self.instance > 1 {
                let _ = self
                    .bootstrap
                    .client
                    .request_snapshot(FrontendRevision::ZERO);
            }

            let client = self.bootstrap.client.clone();
            let config = launch_config(
                &self.bootstrap,
                session_id.clone(),
                self.interrupt_rx.clone(),
            );
            let report = match catch_unwind(AssertUnwindSafe(|| {
                self.runtime.block_on(run_async(client, config))
            })) {
                Ok(report) => report,
                Err(_) => TuiRunReport {
                    outcome: TuiOutcome::FrontendRestartRequested {
                        fault: "frontend panicked".to_owned(),
                    },
                    restore: RestoreReport::default(),
                },
            };
            // Restore is always reported before the exit/restart decision.
            report_restore(&report.restore);

            match report.outcome {
                TuiOutcome::UserExit => {
                    detach(&self.bootstrap, instance_id, DetachReason::UserExit);
                    return Ok(self.shutdown(ExitCode::SUCCESS));
                }
                TuiOutcome::ProcessShutdownRequested => {
                    detach(&self.bootstrap, instance_id, DetachReason::UserExit);
                    return Ok(self.shutdown(ExitCode::SUCCESS));
                }
                TuiOutcome::FrontendRestartRequested { fault: _ } => {
                    last_instance = Some(instance_id);
                    if !self.budget.allow() {
                        return Ok(self.fallback("frontend restart budget exhausted", session_id));
                    }
                }
                TuiOutcome::FallbackRequested { fault } => {
                    detach(
                        &self.bootstrap,
                        instance_id,
                        DetachReason::Fault {
                            message: fault.clone(),
                        },
                    );
                    return Ok(self.fallback(&fault, session_id));
                }
                TuiOutcome::ForcedExitRequested { code } => {
                    detach(
                        &self.bootstrap,
                        instance_id,
                        DetachReason::Fault {
                            message: format!("forced exit {code}"),
                        },
                    );
                    report_forced_exit();
                    return Ok(self.abandon(ExitCode::from(code)));
                }
            }
        }
    }

    fn fallback(self, fault: &str, session_id: String) -> ExitCode {
        eprintln!("error: falling back to line output: {fault}");
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
        let code = ExitCode::from(report.code);
        if report.forced {
            self.abandon(code)
        } else {
            self.shutdown(code)
        }
    }

    fn shutdown(self, code: ExitCode) -> ExitCode {
        let Self {
            runtime, bootstrap, ..
        } = self;
        runtime.block_on(assembly::shutdown(bootstrap));
        code
    }

    /// Forced path: restore already happened. Do not wait for actor drain.
    fn abandon(self, code: ExitCode) -> ExitCode {
        let Self {
            runtime, bootstrap, ..
        } = self;
        drop(bootstrap);
        drop(runtime);
        code
    }
}

fn detach(bootstrap: &Bootstrap, instance_id: FrontendInstanceId, reason: DetachReason) {
    let _ = bootstrap
        .client
        .try_command(FrontendCommand::FrontendDetached {
            frontend_instance_id: instance_id,
            reason,
        });
}

fn report_restore(restore: &RestoreReport) {
    render::write_outputs(&restore_line_outputs(restore));
}

fn report_forced_exit() {
    render::write_outputs(&[forced_exit_notice()]);
}

fn restore_line_outputs(restore: &RestoreReport) -> Vec<Output> {
    restore
        .errors
        .iter()
        .map(|error| Output {
            channel: Channel::Stderr,
            text: format!("terminal restore: {error}\n"),
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
) -> TuiLaunchConfig {
    TuiLaunchConfig {
        session_id,
        model_name: bootstrap.settings.deployment.model.clone(),
        verbose: bootstrap.settings.verbosity == Verbosity::Verbose,
        show_reasoning: bootstrap.settings.show_reasoning,
        context_window: bootstrap.settings.context_window,
        screen: bootstrap.settings.screen,
        interrupt: Some(interrupt),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_budget_allows_three_faults_then_denies() {
        let mut budget = RestartBudget::new();
        assert!(budget.allow());
        assert!(budget.allow());
        assert!(budget.allow());
        assert!(!budget.allow());
    }

    #[test]
    fn restore_errors_precede_forced_exit_notice() {
        let restore = RestoreReport {
            restored: true,
            skipped_stale: false,
            errors: vec!["raw mode failed".to_owned(), "mouse capture".to_owned()],
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
    fn forced_exit_outcome_does_not_use_graceful_drain() {
        assert!(matches!(
            TuiOutcome::ForcedExitRequested { code: 130 },
            TuiOutcome::ForcedExitRequested { .. }
        ));
        let restore = RestoreReport {
            restored: true,
            skipped_stale: false,
            errors: vec!["leave alternate".to_owned()],
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
}
