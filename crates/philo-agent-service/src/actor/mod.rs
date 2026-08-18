//! Service actor: JoinSet child supervision, Runtime consumption, frontend commands.

mod catalog;
mod commands;
mod snapshot;

use snapshot::SnapshotFence;

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use philo_agent_runtime::AgentEvent;
use philo_session::{SessionError, SessionId, SessionStore};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::bounds::{RUNTIME_DRIVER_EVENT_BUDGET, RUNTIME_QUEUE_MAX, STORE_COMMAND_CAP};
use crate::confirmation::{ConfirmationMap, ConfirmationSubmit};
use crate::error::CommandReject;
use crate::frontend::command::{ConfirmationDecision, FrontendCommand};
use crate::frontend::snapshot::{
    FrontendAvailability, FrontendMaintenance, FrontendMaintenancePhase, QueuedOperationSummary,
    ServiceHealth,
};
use crate::frontend::update::{FrontendUpdate, FrontendUpdateKind};
use crate::frontend::{CommandEnvelope, FrontendFeed};
use crate::generation::{
    AssembleError, AssembledGeneration, CurrentGeneration, GenerationAssembler,
};
use crate::ids::{FrontendEpoch, FrontendInstanceId, FrontendRequestId, FrontendRevision};
use crate::live::{LiveOperationSnapshot, LiveToolProgress};
use crate::mapping;
use crate::runtime_api::{RuntimeEvents, RuntimePort};
use philo_agent_runtime::{
    AdmissionError, CancelResult, MaintenanceAccepted, MaintenanceError, MaintenanceResult,
    OperationAccepted, OperationStatus, RuntimeEpoch, RuntimeEvent, RuntimeGeneration,
    RuntimeSnapshot, SettlementDurability, SettlementRevision, ShutdownMode, ShutdownReport,
    ShutdownState, TryRecvError,
};

/// Graceful shutdown phases. Forced deadlines belong to the process supervisor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ServiceShutdownState {
    Running,
    StopAccepting,
    RuntimeDraining,
    ChildrenJoining,
    Stopped,
}

pub(crate) enum ViewKind {
    Snapshot(SnapshotFence),
    Load,
    Preview { request_generation: u64 },
}

pub(crate) enum ServiceTaskResult {
    StoreView {
        request_id: FrontendRequestId,
        epoch: FrontendEpoch,
        kind: ViewKind,
        session_id: String,
        result: Result<philo_session::SessionContextView, String>,
    },
    Install {
        request_id: FrontendRequestId,
        epoch: FrontendEpoch,
        name: String,
        result: Result<AssembledGeneration, AssembleError>,
    },
    Submit {
        request_id: FrontendRequestId,
        epoch: FrontendEpoch,
        result: Result<OperationAccepted, AdmissionError>,
    },
    Cancel {
        request_id: FrontendRequestId,
        result: CancelResult,
    },
    Compaction {
        request_id: FrontendRequestId,
        result: Result<MaintenanceAccepted, MaintenanceError>,
    },
    CancelMaintenance {
        request_id: FrontendRequestId,
        result: CancelResult,
    },
    Shutdown(#[allow(dead_code)] Result<ShutdownReport, philo_agent_runtime::ShutdownError>),
    RuntimeSnapshot {
        epoch: FrontendEpoch,
        snapshot: RuntimeSnapshot,
    },
    ListSessions {
        request_id: FrontendRequestId,
        epoch: FrontendEpoch,
        result: Result<Vec<SessionId>, SessionError>,
    },
}

pub(crate) struct AgentServiceActor<R, S> {
    runtime: R,
    subscription: S,
    sessions: Arc<dyn SessionStore>,
    assembler: Arc<dyn GenerationAssembler>,
    generation: CurrentGeneration,
    live: LiveOperationSnapshot,
    queued: Vec<QueuedOperationSummary>,
    maintenance: Option<FrontendMaintenance>,
    availability: FrontendAvailability,
    confirmations: ConfirmationMap,
    feed: FrontendFeed,
    epoch: FrontendEpoch,
    revision: FrontendRevision,
    current_session: Option<String>,
    attached: Option<FrontendInstanceId>,
    health: ServiceHealth,
    notices: Vec<String>,
    latest_snapshot: Option<FrontendRequestId>,
    latest_preview: Option<(FrontendRequestId, u64)>,
    snapshot_token: u64,
    applied_runtime_revision: u64,
    required_session_revision: HashMap<String, u64>,
    operation_session: HashMap<String, String>,
    runtime_snapshot_inflight: bool,
    session_seq: u64,
    runtime_closed: bool,
    child_tasks: JoinSet<ServiceTaskResult>,
    shutdown: ServiceShutdownState,
}

impl<R, S> AgentServiceActor<R, S>
where
    R: RuntimePort,
    S: RuntimeEvents,
{
    pub(crate) fn new(
        runtime: R,
        subscription: S,
        sessions: Arc<dyn SessionStore>,
        assembler: Arc<dyn GenerationAssembler>,
        initial_generation: Arc<RuntimeGeneration>,
        confirmations: ConfirmationMap,
        feed: FrontendFeed,
    ) -> Self {
        Self {
            runtime,
            subscription,
            sessions,
            assembler,
            generation: CurrentGeneration::new(initial_generation),
            live: LiveOperationSnapshot::new(),
            queued: Vec::new(),
            maintenance: None,
            availability: FrontendAvailability::Idle,
            confirmations,
            feed,
            epoch: FrontendEpoch::INITIAL,
            revision: FrontendRevision::ZERO,
            current_session: None,
            attached: None,
            health: ServiceHealth::Ok,
            notices: Vec::new(),
            latest_snapshot: None,
            latest_preview: None,
            snapshot_token: 0,
            applied_runtime_revision: 0,
            required_session_revision: HashMap::new(),
            operation_session: HashMap::new(),
            runtime_snapshot_inflight: false,
            session_seq: 0,
            runtime_closed: false,
            child_tasks: JoinSet::new(),
            shutdown: ServiceShutdownState::Running,
        }
    }

    pub(crate) async fn run(
        mut self,
        mut command_rx: mpsc::Receiver<CommandEnvelope>,
        mut control_rx: mpsc::Receiver<CommandEnvelope>,
        mut snapshot_rx: mpsc::Receiver<CommandEnvelope>,
        mut confirm_rx: mpsc::Receiver<ConfirmationSubmit>,
    ) {
        let mut command_closed = false;
        let mut control_closed = false;
        let mut snapshot_closed = false;
        let mut confirm_closed = false;

        loop {
            tokio::select! {
                biased;

                envelope = control_rx.recv(), if !control_closed => {
                    match envelope {
                        Some(envelope) => self.handle_command(envelope),
                        None => control_closed = true,
                    }
                }
                result = self.child_tasks.join_next(), if !self.child_tasks.is_empty() => {
                    match result {
                        Some(Ok(result)) => self.handle_task_result(result),
                        Some(Err(_)) => self.on_child_join_error(),
                        None => {}
                    }
                }
                envelope = snapshot_rx.recv(), if !snapshot_closed => {
                    match envelope {
                        Some(envelope) => self.handle_command(envelope),
                        None => snapshot_closed = true,
                    }
                }
                envelope = command_rx.recv(), if !command_closed => {
                    match envelope {
                        Some(envelope) => self.handle_command(envelope),
                        None => command_closed = true,
                    }
                }
                submit = confirm_rx.recv(), if !confirm_closed => {
                    match submit {
                        Some(submit) => self.handle_confirmation_submit(submit),
                        None => confirm_closed = true,
                    }
                }
                event = self.subscription.recv(), if !self.runtime_closed => {
                    match event {
                        Some(event) => {
                            self.handle_runtime_event(event);
                            self.drain_runtime_budget();
                        }
                        None => self.on_runtime_disconnected(),
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)),
                    if self.feed.pending_resync() =>
                {
                    self.feed.flush_resync(self.epoch, self.revision);
                }
            }

            self.feed.flush_resync(self.epoch, self.revision);
            if self.should_stop() {
                break;
            }
        }
    }

    fn drain_runtime_budget(&mut self) {
        for _ in 1..RUNTIME_DRIVER_EVENT_BUDGET {
            match self.subscription.try_recv() {
                Ok(event) => self.handle_runtime_event(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => {
                    self.on_runtime_disconnected();
                    break;
                }
            }
        }
    }

    fn handle_command(&mut self, envelope: CommandEnvelope) {
        let CommandEnvelope {
            request_id,
            command,
        } = envelope;
        match command {
            FrontendCommand::Submit { draft, attachments } => {
                self.handle_submit(request_id, draft, attachments)
            }
            FrontendCommand::CancelOperation { operation_id } => {
                self.spawn_cancel(request_id, operation_id);
            }
            FrontendCommand::StartCompaction { session_id } => {
                self.spawn_compaction(request_id, session_id);
            }
            FrontendCommand::CancelMaintenance { maintenance_id } => {
                self.spawn_cancel_maintenance(request_id, maintenance_id);
            }
            FrontendCommand::ListSessions => self.handle_list_sessions(request_id),
            FrontendCommand::LoadSession { session_id } => {
                self.handle_load_session(request_id, session_id);
            }
            FrontendCommand::PreviewSession {
                session_id,
                request_generation,
            } => self.handle_preview_session(request_id, session_id, request_generation),
            FrontendCommand::CreateSession => self.handle_create_session(request_id),
            FrontendCommand::InstallModel { name } => self.handle_install_model(request_id, name),
            FrontendCommand::SetReasoning { effort } => {
                self.handle_set_reasoning(request_id, effort)
            }
            FrontendCommand::ReadConfig => {
                let entries = mapping::config_entries(&self.generation.current());
                self.emit(
                    Some(request_id),
                    FrontendUpdateKind::ConfigChanged { entries },
                );
            }
            FrontendCommand::ReadStatus => {
                let status = mapping::status(
                    &self.generation.current(),
                    self.availability.clone(),
                    self.queued.len(),
                );
                self.emit(Some(request_id), FrontendUpdateKind::StatusReady(status));
            }
            FrontendCommand::RespondConfirmation {
                confirmation_id,
                decision,
            } => match self.confirmations.respond(confirmation_id, decision) {
                Some(decision) => self.emit(
                    Some(request_id),
                    FrontendUpdateKind::ConfirmationResolved {
                        confirmation_id,
                        decision,
                    },
                ),
                None => self.emit(
                    Some(request_id),
                    FrontendUpdateKind::CommandRejected {
                        reason: CommandReject::UnknownConfirmation,
                    },
                ),
            },
            FrontendCommand::RequestSnapshot { known_revision: _ } => {
                self.handle_request_snapshot(request_id);
            }
            FrontendCommand::FrontendAttached {
                frontend_instance_id,
            } => self.handle_frontend_attached(request_id, frontend_instance_id),
            FrontendCommand::FrontendDetached {
                frontend_instance_id,
                reason,
            } => self.handle_frontend_detached(request_id, frontend_instance_id, reason),
            FrontendCommand::ShutdownRequested => self.handle_shutdown_requested(request_id),
        }
    }

    fn handle_shutdown_requested(&mut self, request_id: FrontendRequestId) {
        match self.shutdown {
            ServiceShutdownState::Running => {
                self.shutdown = ServiceShutdownState::StopAccepting;
                self.health = ServiceHealth::ShuttingDown;
                self.deny_all_confirmations();
                self.emit(
                    Some(request_id),
                    FrontendUpdateKind::ServiceHealthChanged {
                        health: ServiceHealth::ShuttingDown,
                    },
                );
                self.spawn_shutdown();
                self.shutdown = ServiceShutdownState::RuntimeDraining;
            }
            ServiceShutdownState::Stopped => self.emit(
                Some(request_id),
                FrontendUpdateKind::CommandRejected {
                    reason: CommandReject::NotAccepting,
                },
            ),
            _ => self.emit(Some(request_id), FrontendUpdateKind::CommandAccepted),
        }
    }

    fn spawn_shutdown(&mut self) {
        let runtime = self.runtime.clone();
        self.child_tasks.spawn(async move {
            let report = runtime
                .shutdown(
                    ShutdownMode::Drain,
                    Instant::now() + Duration::from_secs(30),
                )
                .await;
            ServiceTaskResult::Shutdown(report)
        });
    }

    fn handle_task_result(&mut self, result: ServiceTaskResult) {
        match result {
            ServiceTaskResult::StoreView {
                request_id,
                epoch,
                kind,
                session_id,
                result,
            } => self.handle_store_view(request_id, epoch, kind, session_id, result),
            ServiceTaskResult::Install {
                request_id,
                epoch,
                name,
                result,
            } => self.handle_install(request_id, epoch, name, result),
            ServiceTaskResult::Submit {
                request_id,
                epoch,
                result,
            } => {
                if epoch != self.epoch {
                    return;
                }
                match result {
                    Ok(accepted) => self.emit(
                        Some(request_id),
                        FrontendUpdateKind::SubmitAccepted {
                            operation_id: accepted.operation_id.to_string(),
                            turn_id: accepted.turn_id.to_string(),
                        },
                    ),
                    Err(error) => self.emit(
                        Some(request_id),
                        FrontendUpdateKind::CommandRejected {
                            reason: CommandReject::AdmissionFailed {
                                message: error.message().to_owned(),
                            },
                        },
                    ),
                }
            }
            ServiceTaskResult::Cancel { request_id, result } => {
                self.emit_cancel(request_id, result)
            }
            ServiceTaskResult::Compaction { request_id, result } => match result {
                Ok(accepted) => self.emit(
                    Some(request_id),
                    FrontendUpdateKind::CompactionAccepted {
                        maintenance_id: accepted.id.to_string(),
                    },
                ),
                Err(error) => self.emit(
                    Some(request_id),
                    FrontendUpdateKind::CommandRejected {
                        reason: CommandReject::AdmissionFailed {
                            message: error.message().to_owned(),
                        },
                    },
                ),
            },
            ServiceTaskResult::CancelMaintenance { request_id, result } => {
                self.emit_cancel(request_id, result);
            }
            ServiceTaskResult::Shutdown(result) => self.on_runtime_shutdown_finished(result),
            ServiceTaskResult::RuntimeSnapshot { epoch, snapshot } => {
                self.handle_runtime_snapshot(epoch, snapshot)
            }
            ServiceTaskResult::ListSessions {
                request_id,
                epoch,
                result,
            } => self.handle_list_sessions_result(request_id, epoch, result),
        }
        self.finish_if_children_idle();
    }

    fn on_child_join_error(&mut self) {
        self.notice("service child task panicked".into());
        if matches!(
            self.shutdown,
            ServiceShutdownState::StopAccepting | ServiceShutdownState::RuntimeDraining
        ) {
            self.on_runtime_shutdown_finished(Err(
                philo_agent_runtime::ShutdownError::SupervisorPanicked,
            ));
        }
        self.finish_if_children_idle();
    }

    fn on_runtime_shutdown_finished(
        &mut self,
        result: Result<ShutdownReport, philo_agent_runtime::ShutdownError>,
    ) {
        match result {
            Ok(report) if report.final_state == ShutdownState::Stopped => {}
            Ok(report) => self.notice(format!(
                "runtime shutdown ended in {:?}",
                report.final_state
            )),
            Err(philo_agent_runtime::ShutdownError::DeadlineExceeded { pending }) => {
                let message = format!("runtime shutdown deadline exceeded: {}", pending.join(","));
                self.health = ServiceHealth::Degraded {
                    message: message.clone(),
                };
                self.notice(message);
            }
            Err(error) => {
                let message = format!("runtime shutdown failed: {error:?}");
                self.health = ServiceHealth::Degraded {
                    message: message.clone(),
                };
                self.notice(message);
            }
        }
        if matches!(
            self.shutdown,
            ServiceShutdownState::StopAccepting | ServiceShutdownState::RuntimeDraining
        ) {
            self.shutdown = ServiceShutdownState::ChildrenJoining;
        }
    }

    fn finish_if_children_idle(&mut self) {
        if matches!(self.shutdown, ServiceShutdownState::ChildrenJoining)
            && self.child_tasks.is_empty()
        {
            self.shutdown = ServiceShutdownState::Stopped;
        }
    }

    fn should_stop(&self) -> bool {
        matches!(self.shutdown, ServiceShutdownState::Stopped)
    }

    fn is_accepting_work(&self) -> bool {
        matches!(self.shutdown, ServiceShutdownState::Running) && !self.runtime_closed
    }

    fn can_spawn_work(&self) -> bool {
        self.child_tasks.len() < STORE_COMMAND_CAP
    }

    fn spawn_work(
        &mut self,
        request_id: FrontendRequestId,
        task: impl Future<Output = ServiceTaskResult> + Send + 'static,
    ) -> bool {
        if !self.can_spawn_work() {
            self.emit(
                Some(request_id),
                FrontendUpdateKind::CommandRejected {
                    reason: CommandReject::ChildCapacity,
                },
            );
            return false;
        }
        self.child_tasks.spawn(task);
        true
    }

    fn reject_child_capacity(&mut self, request_id: FrontendRequestId) {
        self.emit(
            Some(request_id),
            FrontendUpdateKind::CommandRejected {
                reason: CommandReject::ChildCapacity,
            },
        );
    }

    fn reject_not_accepting(&mut self, request_id: FrontendRequestId) {
        self.emit(
            Some(request_id),
            FrontendUpdateKind::CommandRejected {
                reason: CommandReject::NotAccepting,
            },
        );
    }

    fn handle_runtime_event(&mut self, event: RuntimeEvent) {
        self.applied_runtime_revision = self.applied_runtime_revision.saturating_add(1);
        match event {
            RuntimeEvent::OperationAccepted {
                operation_id,
                session_id,
                turn_id,
            } => {
                self.operation_session
                    .insert(operation_id.to_string(), session_id.to_string());
                self.live.accept(operation_id.as_str(), turn_id.as_str());
                self.emit(
                    None,
                    FrontendUpdateKind::OperationAccepted {
                        operation_id: operation_id.to_string(),
                        session_id: session_id.to_string(),
                        turn_id: turn_id.to_string(),
                    },
                );
            }
            RuntimeEvent::OperationSettled {
                operation_id,
                session_id,
                status,
                durability,
                session_revision,
            } => self.handle_operation_settled(
                operation_id.as_str(),
                session_id.as_str(),
                status,
                durability,
                session_revision,
            ),
            RuntimeEvent::Agent(event) => self.handle_agent_event(event),
            RuntimeEvent::AvailabilityChanged {
                availability,
                queued: _,
            } => {
                self.availability = mapping::availability(&availability);
                self.emit(
                    None,
                    FrontendUpdateKind::AvailabilityChanged {
                        availability: self.availability.clone(),
                        queued: self.queued.len(),
                    },
                );
            }
            RuntimeEvent::MaintenanceAccepted { id, session_id: _ } => {
                self.maintenance = Some(FrontendMaintenance {
                    id: id.to_string(),
                    phase: FrontendMaintenancePhase::Accepted,
                    message: None,
                });
                self.emit_maintenance();
            }
            RuntimeEvent::MaintenanceStarted { id } => {
                self.maintenance = Some(FrontendMaintenance {
                    id: id.to_string(),
                    phase: FrontendMaintenancePhase::Started,
                    message: None,
                });
                self.emit_maintenance();
            }
            RuntimeEvent::MaintenanceProgress { id, message } => {
                self.maintenance = Some(FrontendMaintenance {
                    id: id.to_string(),
                    phase: FrontendMaintenancePhase::Progress,
                    message: Some(message),
                });
                self.emit_maintenance();
            }
            RuntimeEvent::MaintenanceSettled {
                id,
                session_id: _,
                result,
            } => {
                let (phase, message) = match result {
                    MaintenanceResult::Compacted(report) => (
                        FrontendMaintenancePhase::Settled,
                        Some(format!("{report:?}")),
                    ),
                    MaintenanceResult::Failed(error) => (
                        FrontendMaintenancePhase::Failed,
                        Some(error.message().to_owned()),
                    ),
                    MaintenanceResult::Cancelled => (FrontendMaintenancePhase::Cancelled, None),
                    MaintenanceResult::Panicked { diagnostic_id } => (
                        FrontendMaintenancePhase::Failed,
                        Some(format!("panicked: {diagnostic_id}")),
                    ),
                };
                self.maintenance = Some(FrontendMaintenance {
                    id: id.to_string(),
                    phase,
                    message,
                });
                self.emit_maintenance();
                self.maintenance = None;
            }
            RuntimeEvent::RuntimeFault {
                diagnostic_id,
                message,
            } => {
                self.health = ServiceHealth::Degraded {
                    message: format!("{diagnostic_id}: {message}"),
                };
                self.emit(
                    None,
                    FrontendUpdateKind::ServiceHealthChanged {
                        health: self.health.clone(),
                    },
                );
            }
            RuntimeEvent::SubscriptionLagged { dropped } => {
                self.on_subscription_lagged();
                self.health = ServiceHealth::Degraded {
                    message: format!("subscription lagged; dropped {dropped}"),
                };
                self.emit(
                    None,
                    FrontendUpdateKind::ServiceHealthChanged {
                        health: self.health.clone(),
                    },
                );
            }
            RuntimeEvent::EpochEnded {
                epoch,
                reason: _,
                forced_count,
            } => {
                self.on_epoch_ended(&epoch, forced_count);
            }
            _ => {}
        }
    }

    fn handle_agent_event(&mut self, event: AgentEvent) {
        match &event {
            AgentEvent::OperationQueued { operation_id } => {
                if self.queued.len() >= RUNTIME_QUEUE_MAX {
                    self.queued.remove(0);
                    self.live.mark_lagged();
                }
                let session_id = self
                    .operation_session
                    .get(operation_id.as_str())
                    .cloned()
                    .unwrap_or_else(|| {
                        self.notice(format!(
                            "queued operation {} has no accepted session",
                            operation_id.as_str()
                        ));
                        String::new()
                    });
                self.queued.push(QueuedOperationSummary {
                    operation_id: operation_id.as_str().to_owned(),
                    session_id,
                });
            }
            AgentEvent::OperationStarted { operation_id } => {
                let id = operation_id.as_str();
                self.queued.retain(|item| item.operation_id != id);
                self.live.start_operation(id);
                self.availability = FrontendAvailability::Busy {
                    operation_id: id.to_owned(),
                };
            }
            AgentEvent::TurnStarted { turn_id } => self.live.start_turn(turn_id.as_str()),
            AgentEvent::ModelCallStarted { model_call_id } => {
                self.live.start_model_call(model_call_id.as_str());
            }
            AgentEvent::TextDelta { delta } => self.live.push_text(delta),
            AgentEvent::ReasoningDelta { text, .. } => self.live.push_reasoning(text),
            AgentEvent::ModelUsageUpdated { usage, .. } => {
                self.live.set_usage(mapping::token_usage(*usage));
            }
            AgentEvent::ToolExecutionProgress {
                tool_batch_id,
                tool_call_id,
                index,
                tail,
            } => self.live.set_tool_progress(LiveToolProgress {
                tool_batch_id: tool_batch_id.as_str().to_owned(),
                tool_call_id: tool_call_id.as_str().to_owned(),
                index: *index,
                tail: tail.clone(),
            }),
            AgentEvent::ToolExecutionCompleted { tool_call_id, .. } => {
                self.live.complete_tool(tool_call_id.as_str());
            }
            _ => {}
        }
        if let Some(mapped) = mapping::operation_event(&event) {
            self.emit(None, FrontendUpdateKind::OperationEvent(mapped));
        }
    }

    fn handle_operation_settled(
        &mut self,
        operation_id: &str,
        session_id: &str,
        status: OperationStatus,
        durability: SettlementDurability,
        session_revision: SettlementRevision,
    ) {
        self.queued.retain(|item| item.operation_id != operation_id);
        for confirmation_id in self.confirmations.deny_for_operation(operation_id) {
            self.emit(
                None,
                FrontendUpdateKind::ConfirmationResolved {
                    confirmation_id,
                    decision: ConfirmationDecision::Deny,
                },
            );
        }
        self.live.settle();
        if self.queued.is_empty() {
            self.availability = FrontendAvailability::Idle;
        }
        if let Some(owned) = self.operation_session.get(operation_id)
            && owned != session_id
        {
            self.notice(format!(
                "settlement session {session_id} does not match accepted session {owned}"
            ));
        }
        self.on_operation_settled(session_id, durability, session_revision);
        self.operation_session.remove(operation_id);
        self.emit(
            None,
            FrontendUpdateKind::OperationEvent(
                crate::frontend::snapshot::FrontendOperationEvent::OperationSettled {
                    operation_id: operation_id.to_owned(),
                    session_id: session_id.to_owned(),
                    status: format!("{status:?}"),
                    durability: format!("{durability:?}"),
                    session_revision,
                },
            ),
        );
    }

    fn on_epoch_ended(&mut self, _runtime_epoch: &RuntimeEpoch, forced_count: usize) {
        self.epoch.bump();
        self.runtime_closed = true;
        self.snapshot_token = self.snapshot_token.saturating_add(1);
        self.live.clear();
        self.queued.clear();
        self.maintenance = None;
        self.availability = FrontendAvailability::Idle;
        self.deny_all_confirmations();
        self.health = ServiceHealth::RuntimeEpochEnded {
            message: format!("{forced_count} forced settlement(s)"),
        };
        self.emit(
            None,
            FrontendUpdateKind::ServiceHealthChanged {
                health: self.health.clone(),
            },
        );
        self.feed.force_resync();
        self.feed.flush_resync(self.epoch, self.revision);
    }

    fn on_runtime_disconnected(&mut self) {
        if !self.runtime_closed {
            self.on_epoch_ended(&RuntimeEpoch::new("closed"), 0);
        }
    }

    fn emit_maintenance(&mut self) {
        if let Some(maintenance) = self.maintenance.clone() {
            self.emit(None, FrontendUpdateKind::MaintenanceChanged(maintenance));
        }
    }

    fn emit(&mut self, request_id: Option<FrontendRequestId>, mut kind: FrontendUpdateKind) {
        let revision = self.revision.bump();
        if let FrontendUpdateKind::SnapshotReady(snapshot) = &mut kind {
            snapshot.revision = revision;
            snapshot.epoch = self.epoch;
        }
        let _ = self
            .feed
            .push(FrontendUpdate::new(self.epoch, revision, request_id, kind));
        self.feed.flush_resync(self.epoch, self.revision);
    }

    fn notice(&mut self, text: String) {
        self.notices.push(text);
        if self.notices.len() > 8 {
            self.notices.remove(0);
        }
    }

    fn deny_all_confirmations(&mut self) {
        for confirmation_id in self.confirmations.deny_all() {
            self.emit(
                None,
                FrontendUpdateKind::ConfirmationResolved {
                    confirmation_id,
                    decision: ConfirmationDecision::Deny,
                },
            );
        }
    }
}
