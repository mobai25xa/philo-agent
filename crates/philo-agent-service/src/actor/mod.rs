//! Service actor: JoinSet child supervision, Runtime consumption, frontend commands.

mod catalog;
mod commands;
mod snapshot;

use snapshot::{SnapshotLoadToken, SnapshotState};

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use philo_agent_runtime::AgentEvent;
use philo_session::{SessionError, SessionId, SessionStore};
use tokio::sync::{mpsc, watch};
use tokio::task::{Id as TaskId, JoinSet};

use crate::bounds::{RUNTIME_DRIVER_EVENT_BUDGET, RUNTIME_QUEUE_MAX, STORE_COMMAND_CAP};
use crate::confirmation::{ConfirmationMap, ConfirmationSubmit};
use crate::error::CommandReject;
use crate::frontend::command::{ConfirmationDecision, FrontendCommand};
use crate::frontend::lease::{
    AttachError, DetachError, DetachReport, FrontendLease, FrontendLeaseGeneration,
    SupervisorCommand,
};
use crate::frontend::snapshot::{
    FrontendAvailability, FrontendMaintenance, FrontendMaintenancePhase, QueuedOperationSummary,
    ServiceHealth,
};
use crate::frontend::supervisor::{SupervisorEnvelope, SupervisorReply};
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActiveFrontend {
    id: FrontendInstanceId,
    generation: FrontendLeaseGeneration,
}

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
    Snapshot(SnapshotLoadToken),
    Load(SnapshotLoadToken),
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
    snapshot: SnapshotState,
    attached: Option<ActiveFrontend>,
    next_lease_generation: u64,
    health: ServiceHealth,
    notices: Vec<String>,
    latest_snapshot: Option<FrontendRequestId>,
    latest_preview: Option<(FrontendRequestId, u64)>,
    /// Runtime snapshot cursor. Never used as a JSONL session floor.
    applied_runtime_revision: u64,
    runtime_snapshot_inflight: bool,
    session_seq: u64,
    runtime_closed: bool,
    child_tasks: JoinSet<ServiceTaskResult>,
    child_requests: HashMap<TaskId, Option<FrontendRequestId>>,
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
            snapshot: SnapshotState::new(),
            attached: None,
            next_lease_generation: 1,
            health: ServiceHealth::Ok,
            notices: Vec::new(),
            latest_snapshot: None,
            latest_preview: None,
            applied_runtime_revision: 0,
            runtime_snapshot_inflight: false,
            session_seq: 0,
            runtime_closed: false,
            child_tasks: JoinSet::new(),
            child_requests: HashMap::new(),
            shutdown: ServiceShutdownState::Running,
        }
    }

    pub(crate) async fn run(
        mut self,
        mut command_rx: mpsc::Receiver<CommandEnvelope>,
        mut control_rx: mpsc::Receiver<CommandEnvelope>,
        mut snapshot_rx: mpsc::Receiver<CommandEnvelope>,
        mut confirm_rx: mpsc::Receiver<ConfirmationSubmit>,
        mut supervisor_rx: mpsc::Receiver<SupervisorEnvelope>,
        mut command_hold: Option<watch::Receiver<bool>>,
    ) {
        let mut command_closed = false;
        let mut control_closed = false;
        let mut snapshot_closed = false;
        let mut confirm_closed = false;
        let mut supervisor_closed = false;

        loop {
            let commands_enabled = match &command_hold {
                None => true,
                Some(hold) => *hold.borrow(),
            };
            tokio::select! {
                biased;

                envelope = supervisor_rx.recv(), if !supervisor_closed => {
                    match envelope {
                        Some(envelope) => self.handle_supervisor(envelope),
                        None => supervisor_closed = true,
                    }
                }
                _ = wait_command_release(&mut command_hold),
                    if command_hold.is_some() && !commands_enabled => {}
                envelope = control_rx.recv(), if !control_closed => {
                    match envelope {
                        Some(envelope) => self.handle_command(envelope),
                        None => control_closed = true,
                    }
                }
                result = self.child_tasks.join_next_with_id(), if !self.child_tasks.is_empty() => {
                    match result {
                        Some(Ok((id, result))) => {
                            self.child_requests.remove(&id);
                            self.handle_task_result(result);
                        }
                        Some(Err(join_error)) => {
                            let request_id = self.child_requests.remove(&join_error.id()).flatten();
                            self.on_child_join_error(request_id);
                        }
                        None => {}
                    }
                }
                envelope = snapshot_rx.recv(), if !snapshot_closed => {
                    match envelope {
                        Some(envelope) => self.handle_command(envelope),
                        None => snapshot_closed = true,
                    }
                }
                envelope = command_rx.recv(), if !command_closed && commands_enabled => {
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
            } => {
                let current = self.attached.as_ref().map(|active| active.generation);
                match self
                    .confirmations
                    .respond(confirmation_id, decision, current)
                {
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
                }
            }
            FrontendCommand::RequestSnapshot { known_revision: _ } => {
                self.handle_request_snapshot(request_id);
            }
            FrontendCommand::ShutdownRequested => self.handle_shutdown_requested(request_id),
        }
    }

    fn handle_supervisor(&mut self, envelope: SupervisorEnvelope) {
        let SupervisorEnvelope {
            command,
            reply,
            deadline,
        } = envelope;
        match command {
            SupervisorCommand::AttachFrontend { id } => {
                let _ = reply.send(SupervisorReply::Attach(self.attach_lease(id)));
            }
            SupervisorCommand::DetachFrontend { lease } => {
                let _ = reply.send(SupervisorReply::Detach(self.detach_lease(lease)));
            }
            SupervisorCommand::Shutdown { .. } => {
                self.begin_host_shutdown(None, deadline, true);
                let _ = reply.send(SupervisorReply::Shutdown);
            }
        }
    }

    fn attach_lease(&mut self, id: FrontendInstanceId) -> Result<FrontendLease, AttachError> {
        if !matches!(self.shutdown, ServiceShutdownState::Running) {
            return Err(AttachError::Disconnected);
        }
        match &self.attached {
            Some(active) if active.id == id => Err(AttachError::AlreadyAttached),
            Some(active) => Err(AttachError::FrontendOccupied {
                current: active.id.clone(),
            }),
            None => {
                let generation = FrontendLeaseGeneration::new(self.next_lease_generation);
                self.next_lease_generation = self.next_lease_generation.saturating_add(1);
                let lease = FrontendLease::issue(id.clone(), generation);
                self.attached = Some(ActiveFrontend { id, generation });
                self.health = ServiceHealth::Ok;
                self.emit(
                    None,
                    FrontendUpdateKind::ServiceHealthChanged {
                        health: self.health.clone(),
                    },
                );
                Ok(lease)
            }
        }
    }

    fn detach_lease(&mut self, lease: FrontendLease) -> Result<DetachReport, DetachError> {
        let (frontend_id, generation) = lease.into_parts();
        match &self.attached {
            Some(active) if active.id == frontend_id && active.generation == generation => {
                let denied_confirmations = self.clear_active_frontend();
                Ok(DetachReport {
                    frontend_id,
                    generation,
                    denied_confirmations,
                })
            }
            _ => Err(DetachError::StaleLease),
        }
    }

    fn clear_active_frontend(&mut self) -> usize {
        let Some(active) = self.attached.take() else {
            return 0;
        };
        self.deny_generation_confirmations(active.generation)
    }

    fn deny_generation_confirmations(&mut self, generation: FrontendLeaseGeneration) -> usize {
        let ids = self.confirmations.deny_for_generation(generation);
        for confirmation_id in &ids {
            self.emit(
                None,
                FrontendUpdateKind::ConfirmationResolved {
                    confirmation_id: *confirmation_id,
                    decision: ConfirmationDecision::Deny,
                },
            );
        }
        ids.len()
    }

    fn handle_shutdown_requested(&mut self, request_id: FrontendRequestId) {
        self.begin_host_shutdown(
            Some(request_id),
            Instant::now() + Duration::from_secs(30),
            false,
        );
    }

    fn begin_host_shutdown(
        &mut self,
        request_id: Option<FrontendRequestId>,
        deadline: Instant,
        allow_forced_upgrade: bool,
    ) {
        match self.shutdown {
            ServiceShutdownState::Running => {
                self.shutdown = ServiceShutdownState::StopAccepting;
                self.health = ServiceHealth::ShuttingDown;
                if self.clear_active_frontend() == 0 {
                    self.deny_all_confirmations();
                }
                self.emit(
                    request_id,
                    FrontendUpdateKind::ServiceHealthChanged {
                        health: ServiceHealth::ShuttingDown,
                    },
                );
                let mode = if Instant::now() >= deadline {
                    ShutdownMode::Forced
                } else {
                    ShutdownMode::Drain
                };
                self.spawn_shutdown(mode, deadline);
                self.shutdown = ServiceShutdownState::RuntimeDraining;
            }
            ServiceShutdownState::Stopped => {
                if let Some(request_id) = request_id {
                    self.emit(
                        Some(request_id),
                        FrontendUpdateKind::CommandRejected {
                            reason: CommandReject::NotAccepting,
                        },
                    );
                }
            }
            _ => {
                if allow_forced_upgrade {
                    self.spawn_shutdown(ShutdownMode::Forced, deadline);
                }
                if let Some(request_id) = request_id {
                    self.emit(Some(request_id), FrontendUpdateKind::CommandAccepted);
                }
            }
        }
    }

    fn spawn_shutdown(&mut self, mode: ShutdownMode, deadline: Instant) {
        let runtime = self.runtime.clone();
        let now = Instant::now();
        let mode = if now >= deadline {
            ShutdownMode::Forced
        } else {
            mode
        };
        // RuntimeHandle::shutdown does not post the watch once the deadline has
        // elapsed. Keep a short bound so Forced still reaches the coordinator.
        let deadline = if now >= deadline {
            now + Duration::from_millis(50)
        } else {
            deadline
        };
        self.spawn_child(None, async move {
            let report = runtime.shutdown(mode, deadline).await;
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
                    self.reject_not_accepting(request_id);
                } else {
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

    fn on_child_join_error(&mut self, request_id: Option<FrontendRequestId>) {
        self.notice("service child task panicked".into());
        if let Some(request_id) = request_id {
            self.emit(
                Some(request_id),
                FrontendUpdateKind::CommandRejected {
                    reason: CommandReject::AdmissionFailed {
                        message: "service child task panicked".into(),
                    },
                },
            );
        }
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

    fn spawn_child(
        &mut self,
        request_id: Option<FrontendRequestId>,
        task: impl Future<Output = ServiceTaskResult> + Send + 'static,
    ) {
        let abort = self.child_tasks.spawn(task);
        self.child_requests.insert(abort.id(), request_id);
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
        self.spawn_child(Some(request_id), task);
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
                self.snapshot
                    .note_accepted(operation_id.to_string(), session_id.to_string());
                if self.snapshot.is_current_session(session_id.as_str()) {
                    self.live.accept(operation_id.as_str(), turn_id.as_str());
                }
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
                    .snapshot
                    .session_of(operation_id.as_str())
                    .map(str::to_owned)
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
                self.availability = FrontendAvailability::Busy {
                    operation_id: id.to_owned(),
                };
                if self.snapshot.live_belongs_to_current(Some(id)) {
                    self.live.start_operation(id);
                }
            }
            AgentEvent::TurnStarted { turn_id } => {
                if self.live_accepts_agent_events() {
                    self.live.start_turn(turn_id.as_str());
                }
            }
            AgentEvent::ModelCallStarted { model_call_id } => {
                if self.live_accepts_agent_events() {
                    self.live.start_model_call(model_call_id.as_str());
                }
            }
            AgentEvent::TextDelta { delta } => {
                if self.live_accepts_agent_events() {
                    self.live.push_text(delta);
                }
            }
            AgentEvent::ReasoningDelta { text, .. } => {
                if self.live_accepts_agent_events() {
                    self.live.push_reasoning(text);
                }
            }
            AgentEvent::ModelUsageUpdated { usage, .. } => {
                if self.live_accepts_agent_events() {
                    self.live.set_usage(mapping::token_usage(*usage));
                }
            }
            AgentEvent::ToolExecutionProgress {
                tool_batch_id,
                tool_call_id,
                index,
                tail,
            } => {
                if self.live_accepts_agent_events() {
                    self.live.set_tool_progress(LiveToolProgress {
                        tool_batch_id: tool_batch_id.as_str().to_owned(),
                        tool_call_id: tool_call_id.as_str().to_owned(),
                        index: *index,
                        tail: tail.clone(),
                    });
                }
            }
            AgentEvent::ToolExecutionCompleted { tool_call_id, .. } => {
                if self.live_accepts_agent_events() {
                    self.live.complete_tool(tool_call_id.as_str());
                }
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
        if self.live.operation_id.as_deref() == Some(operation_id) {
            self.live.settle();
        }
        if self.queued.is_empty() && self.live.operation_id.is_none() {
            self.availability = FrontendAvailability::Idle;
        }
        if !self.apply_operation_settled(operation_id, session_id, durability, session_revision) {
            return;
        }
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
        self.snapshot.on_epoch_reset();
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

    fn current_lease_generation(&self) -> Option<FrontendLeaseGeneration> {
        self.attached.as_ref().map(|active| active.generation)
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

async fn wait_command_release(hold: &mut Option<watch::Receiver<bool>>) {
    let Some(rx) = hold.as_mut() else {
        std::future::pending::<()>().await;
        return;
    };
    if *rx.borrow() {
        return;
    }
    let _ = rx.changed().await;
}
