//! Real-time one-shot / fallback frontend: continuation notice, event
//! output, Ctrl+C policy, and terminal outcome mapping.
//!
//! Consumes `FrontendClient` only. Never polls `RuntimeHandle` for events.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use philo_agent_runtime::{UserMessage, UserPart};
use philo_agent_service::{
    CommandSubmitResult, FrontendAttachment, FrontendAvailability, FrontendClient, FrontendCommand,
    FrontendOperationEvent, FrontendUpdateKind, RecvOutcome,
};
use philo_session::SessionStore;
use philo_session_jsonl::JsonlSessionStore;
use tokio::sync::watch;

use crate::command::ctrl_c::{self, CtrlCDecision, CtrlCPhase};
use crate::config::Verbosity;
use crate::render::{self, Channel, Output, Renderer};

pub(crate) struct Request {
    pub client: FrontendClient,
    pub sessions: Option<Arc<JsonlSessionStore>>,
    pub session_id: String,
    pub continues_existing: bool,
    pub user_message: Option<UserMessage>,
    pub verbosity: Verbosity,
    pub show_reasoning: bool,
    /// Exit code used when the operation succeeded (0 for oneshot, 1 for fallback).
    pub success_exit: u8,
    /// Supervisor/oneshot-owned Ctrl+C pulse counter.
    pub interrupt: watch::Receiver<u64>,
}

/// Terminal result of a oneshot/fallback drive. `forced` skips actor drain.
pub(crate) struct DriveReport {
    pub code: u8,
    pub forced: bool,
}

impl DriveReport {
    fn graceful(code: u8) -> Self {
        Self {
            code,
            forced: false,
        }
    }

    fn forced(code: u8) -> Self {
        Self { code, forced: true }
    }

    pub(crate) fn exit_code(&self) -> ExitCode {
        ExitCode::from(self.code)
    }
}

pub(crate) async fn run(mut request: Request) -> DriveReport {
    let quiet = request.verbosity == Verbosity::Quiet;

    // Presentation-only heuristic over the public context view.
    if request.continues_existing && !quiet {
        if let Some(sessions) = &request.sessions {
            let stored_id = philo_session::SessionId::new(request.session_id.as_str());
            if let Ok(view) = sessions.context_view(&stored_id).await {
                let unfinished = view.messages().last().is_some_and(|message| {
                    !matches!(message, philo_session::ContextMessage::Assistant { .. })
                });
                if unfinished {
                    eprintln!(
                        "note: the previous turn did not finish normally; its partial \
                         trajectory remains in the context"
                    );
                }
            }
        }
    }

    let submitting = request.user_message.is_some();
    if let Some(message) = &request.user_message {
        match submit(&request.client, &request.session_id, message).await {
            Ok(()) => {}
            Err(code) => return DriveReport::graceful(code),
        }
    } else {
        let _ = request
            .client
            .request_snapshot(philo_agent_service::FrontendRevision::ZERO);
    }

    let mut renderer = Renderer::new(request.verbosity).with_reasoning(request.show_reasoning);
    let mut phase = if submitting {
        CtrlCPhase::Busy { operation_id: None }
    } else {
        CtrlCPhase::Idle
    };
    let mut interrupt_seen = ctrl_c::skip_past_pulses(&mut request.interrupt);
    let mut interrupt_open = true;
    let mut operation_id: Option<String> = None;
    let mut settled: Option<(String, String)> = None;
    let mut rejected: Option<String> = None;
    let mut fallback_idle = false;
    let mut cancel_notice = false;

    loop {
        if settled.is_some() || rejected.is_some() || fallback_idle {
            break;
        }
        enum Step {
            Outcome(RecvOutcome),
            Interrupt,
        }
        let interrupt_wait = async {
            if interrupt_open {
                request.interrupt.changed().await
            } else {
                std::future::pending::<Result<(), watch::error::RecvError>>().await
            }
        };
        let step = tokio::select! {
            outcome = request.client.recv_until_async(Instant::now() + Duration::from_secs(3600)) => {
                Step::Outcome(outcome)
            }
            changed = interrupt_wait => {
                if changed.is_err() {
                    interrupt_open = false;
                    continue;
                }
                Step::Interrupt
            }
        };
        match step {
            Step::Outcome(RecvOutcome::Update(update)) => match &update.kind {
                FrontendUpdateKind::OperationAccepted {
                    operation_id: id, ..
                } => {
                    operation_id = Some(id.clone());
                    let waiting = matches!(phase, CtrlCPhase::Cancelling { operation_id: None });
                    phase.observe_busy(id.clone());
                    if waiting {
                        if let Some(pending) = phase.pending_cancel_id() {
                            let _ = enqueue(
                                &request.client,
                                FrontendCommand::CancelOperation {
                                    operation_id: pending.to_owned(),
                                },
                            )
                            .await;
                        }
                    }
                }
                FrontendUpdateKind::CommandRejected { reason } => {
                    rejected = Some(reason.clone());
                    render::write_outputs(&renderer.render_update(&update.kind));
                }
                FrontendUpdateKind::SnapshotReady(snapshot) if !submitting => {
                    match &snapshot.availability {
                        FrontendAvailability::Idle if snapshot.live.is_empty() => {
                            fallback_idle = true;
                            phase.observe_idle();
                        }
                        FrontendAvailability::Busy { operation_id: id } => {
                            operation_id = Some(id.clone());
                            phase.observe_busy(id.clone());
                        }
                        _ => {}
                    }
                }
                FrontendUpdateKind::AvailabilityChanged {
                    availability: FrontendAvailability::Busy { operation_id: id },
                    ..
                } => {
                    operation_id = Some(id.clone());
                    phase.observe_busy(id.clone());
                }
                FrontendUpdateKind::AvailabilityChanged {
                    availability: FrontendAvailability::Idle,
                    ..
                } => {
                    operation_id = None;
                    phase.observe_idle();
                }
                FrontendUpdateKind::OperationEvent(FrontendOperationEvent::OperationStarted {
                    operation_id: id,
                })
                | FrontendUpdateKind::OperationEvent(FrontendOperationEvent::OperationQueued {
                    operation_id: id,
                }) => {
                    operation_id = Some(id.clone());
                    phase.observe_busy(id.clone());
                    render::write_outputs(&renderer.render_update(&update.kind));
                }
                FrontendUpdateKind::OperationEvent(FrontendOperationEvent::OperationSettled {
                    status,
                    durability,
                    ..
                }) => {
                    settled = Some((status.clone(), durability.clone()));
                    phase.observe_idle();
                    render::write_outputs(&renderer.render_update(&update.kind));
                }
                _ => render::write_outputs(&renderer.render_update(&update.kind)),
            },
            Step::Outcome(RecvOutcome::Disconnected) => {
                eprintln!("error: the service disconnected before the operation settled");
                return DriveReport::graceful(1);
            }
            Step::Outcome(RecvOutcome::Timeout) => {}
            Step::Interrupt => {
                let delta = ctrl_c::take_pulses(&mut request.interrupt, &mut interrupt_seen);
                for _ in 0..delta {
                    match phase.on_ctrl_c() {
                        CtrlCDecision::UserExit => {
                            return DriveReport::graceful(if submitting { 130 } else { 1 });
                        }
                        CtrlCDecision::Cancel { operation_id: id } => {
                            show_cancel_notice(&mut cancel_notice);
                            let _ = enqueue(
                                &request.client,
                                FrontendCommand::CancelOperation { operation_id: id },
                            )
                            .await;
                        }
                        CtrlCDecision::WaitForId => {
                            show_cancel_notice(&mut cancel_notice);
                            if let Some(id) = &operation_id {
                                let _ = enqueue(
                                    &request.client,
                                    FrontendCommand::CancelOperation {
                                        operation_id: id.clone(),
                                    },
                                )
                                .await;
                            }
                        }
                        CtrlCDecision::ForcedExit { code } => {
                            render::write_outputs(&[Output {
                                channel: Channel::Stderr,
                                text: "forced exit: the session state may be unconfirmed\n"
                                    .to_owned(),
                            }]);
                            return DriveReport::forced(code);
                        }
                    }
                }
            }
        }
    }

    if rejected.is_some() || fallback_idle {
        return DriveReport::graceful(1);
    }
    match settled
        .as_ref()
        .map(|(status, durability)| (status.as_str(), durability.as_str()))
    {
        Some(("Cancelled", _)) => DriveReport::graceful(130),
        Some(("Succeeded", _)) => DriveReport::graceful(request.success_exit),
        Some(_) | None => DriveReport::graceful(1),
    }
}

fn show_cancel_notice(shown: &mut bool) {
    if *shown {
        return;
    }
    *shown = true;
    eprintln!(
        "\ncancelling: waiting for the orderly settlement \
         (press Ctrl+C again to force quit)"
    );
}

async fn submit(
    client: &FrontendClient,
    session_id: &str,
    message: &UserMessage,
) -> Result<(), u8> {
    enqueue(client, submit_command(session_id, message)).await
}

fn submit_command(session_id: &str, message: &UserMessage) -> FrontendCommand {
    let mut draft = String::new();
    let mut attachments = Vec::new();
    for part in message.parts() {
        match part {
            UserPart::Text(text) => {
                if !draft.is_empty() {
                    draft.push('\n');
                }
                draft.push_str(text);
            }
            UserPart::Image { media_type, bytes } => attachments.push(FrontendAttachment {
                media_type: media_type.clone(),
                bytes: bytes.clone(),
            }),
        }
    }
    FrontendCommand::Submit {
        session_id: session_id.to_owned(),
        draft,
        attachments,
    }
}

async fn enqueue(client: &FrontendClient, command: FrontendCommand) -> Result<(), u8> {
    loop {
        match client.try_command(command.clone()) {
            CommandSubmitResult::Accepted(_) => return Ok(()),
            CommandSubmitResult::Backpressured => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            CommandSubmitResult::Disconnected => {
                eprintln!("error: the service disconnected");
                return Err(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use philo_agent_runtime::{UserMessage, UserPart};
    use tokio::sync::watch;

    fn text_message(text: &str) -> UserMessage {
        UserMessage::from_parts(vec![UserPart::Text(text.to_owned())]).expect("text")
    }

    #[tokio::test]
    async fn first_ctrl_c_cancels_second_returns_forced_130() {
        let (service, client, runtime) = philo_agent_service::testing::start_test_service();
        let _hold = runtime.hold_children();
        let (tx, rx) = watch::channel(0u64);
        let drive = run(Request {
            client,
            sessions: None,
            session_id: "s-1".to_owned(),
            continues_existing: false,
            user_message: Some(text_message("hi")),
            verbosity: Verbosity::Quiet,
            show_reasoning: false,
            success_exit: 0,
            interrupt: rx,
        });
        let task = tokio::spawn(drive);
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        ctrl_c::pulse(&tx);
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !task.is_finished(),
            "first Ctrl+C must cancel and keep waiting"
        );
        ctrl_c::pulse(&tx);
        let report = task.await.expect("drive join");
        assert!(report.forced);
        assert_eq!(report.code, ctrl_c::FORCED_EXIT_CODE);
        drop(service);
    }

    #[tokio::test]
    async fn coalesced_two_pulses_force_exit() {
        let (service, client, runtime) = philo_agent_service::testing::start_test_service();
        let _hold = runtime.hold_children();
        let (tx, rx) = watch::channel(0u64);
        let drive = run(Request {
            client,
            sessions: None,
            session_id: "s-1".to_owned(),
            continues_existing: false,
            user_message: Some(text_message("hi")),
            verbosity: Verbosity::Quiet,
            show_reasoning: false,
            success_exit: 0,
            interrupt: rx,
        });
        let task = tokio::spawn(drive);
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        ctrl_c::pulse(&tx);
        ctrl_c::pulse(&tx);
        let report = task.await.expect("drive join");
        assert!(report.forced);
        assert_eq!(report.code, 130);
        drop(service);
    }
}
