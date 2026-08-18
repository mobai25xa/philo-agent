//! Runtime coordinator actor: exclusive owner of active/queue/maintenance/shutdown.

use crate::catch_unwind::catch_unwind_async;
use crate::engine::{self, EngineContext};
use crate::epoch::{
    AcceptedLedger, EPOCH_CHILD_JOIN_DEADLINE, EpochChildHandoff, EpochChildren, EpochShared,
    abort_and_join,
};
use crate::operation::{DriverEvent, MaintenanceCancel, OperationShared};
use crate::runtime_event::is_mergeable;
use crate::transient::TransientOutbound;
use crate::{
    ActiveOperationSnapshot, AdmissionError, AgentAvailability, AgentEvent, AgentFailure,
    CancelResult, ChannelBounds, CompactionSpec, DiagnosticId, DriverExit, EpochEndReason,
    ForcedSettlement, IdSource, MaintenanceAccepted, MaintenanceError, MaintenanceId,
    MaintenanceResult, MaintenanceSnapshot, OperationAccepted, OperationId, OperationPhase,
    OperationSpec, OperationStatus, QueuedOperationSnapshot, RuntimeEpoch, RuntimeEvent,
    RuntimeSnapshot, SessionId, SettledOperationSnapshot, SettlementDurability, SettlementRevision,
    ShutdownMode, ShutdownReport, ShutdownState, TurnId,
};
use philo_session::CancelReason;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

pub(crate) enum RuntimeCommand {
    Submit {
        spec: OperationSpec,
        reply: oneshot::Sender<Result<OperationAccepted, AdmissionError>>,
    },
    StartCompaction {
        spec: CompactionSpec,
        reply: oneshot::Sender<Result<MaintenanceAccepted, MaintenanceError>>,
    },
}

pub(crate) enum ControlMessage {
    Cancel {
        operation_id: OperationId,
        reply: oneshot::Sender<CancelResult>,
    },
    CancelMaintenance {
        id: MaintenanceId,
        reply: oneshot::Sender<CancelResult>,
    },
    Shutdown {
        mode: ShutdownMode,
        reply: oneshot::Sender<ShutdownReport>,
    },
    InjectCoordinatorPanic,
    PublishSnapshot {
        reply: oneshot::Sender<RuntimeSnapshot>,
    },
}

struct QueuedOperation {
    operation_id: OperationId,
    turn_id: TurnId,
    spec: OperationSpec,
}

struct ActiveMeta {
    operation_id: OperationId,
    turn_id: TurnId,
    session_id: SessionId,
    shared: Arc<OperationShared>,
    phase: OperationPhase,
    started: bool,
    settled_event_seen: bool,
}

struct MaintenanceMeta {
    id: MaintenanceId,
    session_id: SessionId,
    cancel: Arc<MaintenanceCancel>,
}

pub(crate) struct Coordinator {
    epoch: RuntimeEpoch,
    sessions: Arc<dyn philo_session::SessionStore>,
    ids: Arc<dyn IdSource>,
    bounds: ChannelBounds,
    last_input_tokens: Arc<Mutex<HashMap<SessionId, u64>>>,
    next_maintenance: AtomicU64,
    next_diagnostic: AtomicU64,
    command_rx: mpsc::Receiver<RuntimeCommand>,
    control_rx: mpsc::Receiver<ControlMessage>,
    event_tx: mpsc::Sender<RuntimeEvent>,
    snapshot_tx: watch::Sender<RuntimeSnapshot>,
    active: Option<ActiveMeta>,
    driver_events: Option<mpsc::Receiver<DriverEvent>>,
    driver_join: Option<JoinHandle<DriverExit>>,
    queue: VecDeque<QueuedOperation>,
    maintenance: Option<MaintenanceMeta>,
    maintenance_join: Option<JoinHandle<Result<crate::CompactionReport, crate::CompactionError>>>,
    shutdown: ShutdownState,
    shutdown_reply: Option<oneshot::Sender<ShutdownReport>>,
    shutdown_deadline: Option<Instant>,
    pending_reliable: VecDeque<RuntimeEvent>,
    transient_hold: TransientOutbound,
    last_settled: Vec<SettledOperationSnapshot>,
    last_availability: AgentAvailability,
    last_queued_len: usize,
    runtime_revision: u64,
    finished_report: Option<ShutdownReport>,
    ledger: AcceptedLedger,
    children: EpochChildHandoff,
    epoch_ended: Arc<std::sync::atomic::AtomicBool>,
}

impl Coordinator {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn(
        epoch: RuntimeEpoch,
        sessions: Arc<dyn philo_session::SessionStore>,
        ids: Arc<dyn IdSource>,
        bounds: ChannelBounds,
        command_rx: mpsc::Receiver<RuntimeCommand>,
        control_rx: mpsc::Receiver<ControlMessage>,
        event_tx: mpsc::Sender<RuntimeEvent>,
        snapshot_tx: watch::Sender<RuntimeSnapshot>,
        shared: EpochShared,
    ) -> JoinHandle<()> {
        let coordinator = Self {
            epoch: epoch.clone(),
            sessions,
            ids,
            bounds,
            last_input_tokens: Arc::new(Mutex::new(HashMap::new())),
            next_maintenance: AtomicU64::new(1),
            next_diagnostic: AtomicU64::new(1),
            command_rx,
            control_rx,
            event_tx,
            snapshot_tx,
            active: None,
            driver_events: None,
            driver_join: None,
            queue: VecDeque::new(),
            maintenance: None,
            maintenance_join: None,
            shutdown: ShutdownState::Running,
            shutdown_reply: None,
            shutdown_deadline: None,
            pending_reliable: VecDeque::new(),
            transient_hold: TransientOutbound::default(),
            last_settled: Vec::new(),
            last_availability: AgentAvailability::Idle,
            last_queued_len: 0,
            runtime_revision: 0,
            finished_report: None,
            ledger: shared.ledger,
            children: shared.children,
            epoch_ended: shared.epoch_ended,
        };
        tokio::spawn(coordinator.run())
    }

    async fn run(mut self) {
        self.publish_snapshot();
        while !matches!(self.shutdown, ShutdownState::Stopped) {
            self.turn().await;
            if matches!(
                self.shutdown,
                ShutdownState::Draining | ShutdownState::Forced
            ) && self.active.is_none()
                && self.maintenance.is_none()
                && self.queue.is_empty()
            {
                self.finish_epoch().await;
            }
        }
        self.flush_reliable_await().await;
        let report = self
            .finished_report
            .take()
            .unwrap_or_else(|| self.shutdown_report());
        let reply = self.shutdown_reply.take();
        drop(self);
        if let Some(reply) = reply {
            let _ = reply.send(report);
        }
    }

    async fn turn(&mut self) {
        self.pull_caught_up_transients();
        self.flush_outbound();
        if self.reap_finished_children().await {
            self.drain_pending_lanes();
            self.maybe_start_next();
            self.publish_snapshot();
            self.emit_availability_if_changed();
            self.flush_outbound();
            return;
        }
        let budget = self.bounds.driver_event_budget;
        let op_deadline = self.active_deadline();
        let shutdown_deadline = self.shutdown_deadline;
        let event_tx = self.event_tx.clone();
        let has_pending = !self.pending_reliable.is_empty();
        let wait_transients = !self.driver_events_pending();
        tokio::select! {
            biased;
            control = self.control_rx.recv() => self.on_control(control),
            command = self.command_rx.recv() => self.on_command(command),
            event = recv_driver(&mut self.driver_events) => self.on_driver_event(event),
            join = join_driver(&mut self.driver_join) => self.on_driver_join(join),
            join = join_maintenance(&mut self.maintenance_join) => self.on_maintenance_join(join),
            permit = reserve_if_pending(&event_tx, has_pending) => {
                if let Some(Ok(permit)) = permit {
                    if let Some(event) = self.pending_reliable.pop_front() {
                        permit.send(event);
                    }
                }
            }
            _ = wait_driver_transients(&self.active, wait_transients) => {
                self.pull_caught_up_transients()
            }
            _ = sleep_until(op_deadline) => self.on_operation_timeout(),
            _ = sleep_until(shutdown_deadline) => self.on_shutdown_timeout().await,
        }
        for _ in 1..budget {
            match self
                .driver_events
                .as_mut()
                .and_then(|rx| rx.try_recv().ok())
            {
                Some(event) => self.forward_driver_event(event),
                None => break,
            }
        }
        self.pull_caught_up_transients();
        self.maybe_start_next();
        self.publish_snapshot();
        self.emit_availability_if_changed();
        self.flush_outbound();
    }

    async fn reap_finished_children(&mut self) -> bool {
        let mut reaped = false;
        if self
            .driver_join
            .as_ref()
            .is_some_and(|join| join.is_finished())
        {
            let join = self.driver_join.take().expect("finished driver join");
            self.on_driver_join(Some(join.await));
            reaped = true;
        }
        if self
            .maintenance_join
            .as_ref()
            .is_some_and(|join| join.is_finished())
        {
            let join = self
                .maintenance_join
                .take()
                .expect("finished maintenance join");
            self.on_maintenance_join(Some(join.await));
            reaped = true;
        }
        reaped
    }

    fn drain_pending_lanes(&mut self) {
        while let Ok(control) = self.control_rx.try_recv() {
            self.on_control(Some(control));
        }
        while let Ok(command) = self.command_rx.try_recv() {
            self.on_command(Some(command));
        }
    }

    fn on_control(&mut self, control: Option<ControlMessage>) {
        let Some(control) = control else {
            self.begin_shutdown(ShutdownMode::Forced, None);
            return;
        };
        match control {
            ControlMessage::Cancel {
                operation_id,
                reply,
            } => {
                let _ = reply.send(self.cancel_operation(operation_id));
            }
            ControlMessage::CancelMaintenance { id, reply } => {
                let _ = reply.send(self.cancel_maintenance(id));
            }
            ControlMessage::Shutdown { mode, reply } => {
                self.begin_shutdown(mode, Some(reply));
            }
            ControlMessage::InjectCoordinatorPanic => {
                panic!("injected coordinator panic");
            }
            ControlMessage::PublishSnapshot { reply } => {
                self.publish_snapshot();
                let _ = reply.send(self.snapshot_tx.borrow().clone());
            }
        }
    }

    fn on_command(&mut self, command: Option<RuntimeCommand>) {
        let Some(command) = command else {
            if matches!(self.shutdown, ShutdownState::Running) {
                self.begin_shutdown(ShutdownMode::Forced, None);
            }
            return;
        };
        if !matches!(self.shutdown, ShutdownState::Running) {
            match command {
                RuntimeCommand::Submit { reply, .. } => {
                    let _ = reply.send(Err(AdmissionError::ShuttingDown));
                }
                RuntimeCommand::StartCompaction { reply, .. } => {
                    let _ = reply.send(Err(MaintenanceError::ShuttingDown));
                }
            }
            return;
        }
        match command {
            RuntimeCommand::Submit { spec, reply } => {
                let _ = reply.send(self.admit(spec));
            }
            RuntimeCommand::StartCompaction { spec, reply } => {
                let _ = reply.send(self.admit_compaction(spec));
            }
        }
    }

    fn admit(&mut self, spec: OperationSpec) -> Result<OperationAccepted, AdmissionError> {
        let idle = self.active.is_none() && self.maintenance.is_none() && self.queue.is_empty();
        if !idle && self.queue.len() >= self.bounds.queue_max {
            return Err(AdmissionError::QueueFull);
        }
        let operation_id = self.ids.next_operation_id();
        let turn_id = self.ids.next_turn_id();
        let accepted = OperationAccepted {
            operation_id: operation_id.clone(),
            turn_id: turn_id.clone(),
        };
        if self
            .ledger
            .insert(
                operation_id.clone(),
                turn_id.clone(),
                spec.session_id.clone(),
            )
            .is_err()
        {
            return Err(AdmissionError::QueueFull);
        }
        self.emit(RuntimeEvent::OperationAccepted {
            operation_id: operation_id.clone(),
            session_id: spec.session_id.clone(),
            turn_id: turn_id.clone(),
        });
        let queued = QueuedOperation {
            operation_id: operation_id.clone(),
            turn_id,
            spec,
        };
        if idle {
            self.spawn_driver(queued);
        } else {
            self.emit(RuntimeEvent::Agent(AgentEvent::OperationQueued {
                operation_id,
            }));
            self.queue.push_back(queued);
        }
        self.publish_snapshot();
        Ok(accepted)
    }

    fn admit_compaction(
        &mut self,
        spec: CompactionSpec,
    ) -> Result<MaintenanceAccepted, MaintenanceError> {
        if self.active.is_some() || self.maintenance.is_some() || !self.queue.is_empty() {
            return Err(MaintenanceError::Unavailable {
                availability: self.availability(),
            });
        }
        let id = MaintenanceId::new(format!(
            "maintenance-{}",
            self.next_maintenance.fetch_add(1, Ordering::Relaxed)
        ));
        let session_id = spec.session_id.clone();
        self.spawn_maintenance(id.clone(), spec);
        self.emit(RuntimeEvent::MaintenanceAccepted {
            id: id.clone(),
            session_id,
        });
        self.emit(RuntimeEvent::MaintenanceStarted { id: id.clone() });
        self.publish_snapshot();
        Ok(MaintenanceAccepted { id })
    }

    fn cancel_operation(&mut self, operation_id: OperationId) -> CancelResult {
        if self
            .last_settled
            .iter()
            .any(|settled| settled.operation_id == operation_id)
        {
            return CancelResult::AlreadySettled;
        }
        if let Some(index) = self
            .queue
            .iter()
            .position(|queued| queued.operation_id == operation_id)
        {
            let queued = self.queue.remove(index).expect("index came from position");
            self.emit(RuntimeEvent::Agent(AgentEvent::CancellationRequested {
                operation_id: operation_id.clone(),
                reason: CancelReason::User,
            }));
            self.emit(RuntimeEvent::OperationSettled {
                operation_id: operation_id.clone(),
                session_id: queued.spec.session_id.clone(),
                status: OperationStatus::Cancelled,
                durability: SettlementDurability::Confirmed,
                session_revision: SettlementRevision::Unchanged,
            });
            self.record_settled(
                operation_id,
                queued.spec.session_id,
                OperationStatus::Cancelled,
                SettlementDurability::Confirmed,
                None,
            );
            return CancelResult::QueuedCancelled;
        }
        if let Some(active) = &self.active
            && active.operation_id == operation_id
        {
            if matches!(
                active.shared.phase(),
                OperationPhase::Finalizing | OperationPhase::Settled(_)
            ) {
                return CancelResult::TooLate;
            }
            active.shared.request_cancel(CancelReason::User);
            return CancelResult::Requested;
        }
        CancelResult::UnknownOperation
    }

    fn cancel_maintenance(&mut self, id: MaintenanceId) -> CancelResult {
        match &self.maintenance {
            Some(maintenance) if maintenance.id == id => {
                maintenance.cancel.request();
                CancelResult::Requested
            }
            Some(_) => CancelResult::UnknownOperation,
            None => CancelResult::AlreadySettled,
        }
    }

    fn spawn_driver(&mut self, queued: QueuedOperation) {
        let (event_tx, event_rx) = mpsc::channel(self.bounds.event_cap);
        let shared = Arc::new(OperationShared::new(
            queued.operation_id.clone(),
            queued.turn_id.clone(),
            event_tx,
            OperationPhase::PreparingTurn,
        ));
        let ctx = EngineContext {
            generation: queued.spec.generation.clone(),
            sessions: self.sessions.clone(),
            last_input_tokens: self.last_input_tokens.clone(),
            maintenance_cancel: None,
        };
        let driver_shared = shared.clone();
        let session_id = queued.spec.session_id.clone();
        let user_message = queued.spec.user_message.clone();
        let diagnostic_id = self.next_diagnostic();
        let join = tokio::spawn(async move {
            match catch_unwind_async(engine::drive(ctx, driver_shared, session_id, user_message))
                .await
            {
                Ok(exit) => exit,
                Err(_) => DriverExit::Panicked { diagnostic_id },
            }
        });
        self.active = Some(ActiveMeta {
            operation_id: queued.operation_id,
            turn_id: queued.turn_id,
            session_id: queued.spec.session_id,
            shared,
            phase: OperationPhase::PreparingTurn,
            started: false,
            settled_event_seen: false,
        });
        self.driver_events = Some(event_rx);
        self.driver_join = Some(join);
    }

    fn spawn_maintenance(&mut self, id: MaintenanceId, spec: CompactionSpec) {
        let cancel = MaintenanceCancel::new();
        let ctx = EngineContext {
            generation: spec.generation,
            sessions: self.sessions.clone(),
            last_input_tokens: self.last_input_tokens.clone(),
            maintenance_cancel: Some(cancel.clone()),
        };
        let session_id = spec.session_id.clone();
        let join = tokio::spawn(async move {
            match catch_unwind_async(engine::compaction::compact_manually(&ctx, &session_id)).await
            {
                Ok(result) => result,
                Err(_) => Err(crate::CompactionError::Session {
                    message: "compaction driver panicked".to_owned(),
                }),
            }
        });
        self.maintenance = Some(MaintenanceMeta {
            id,
            session_id: spec.session_id,
            cancel,
        });
        self.maintenance_join = Some(join);
    }

    fn on_driver_event(&mut self, event: Option<DriverEvent>) {
        let Some(event) = event else {
            self.driver_events = None;
            return;
        };
        self.forward_driver_event(event);
    }

    fn forward_driver_event(&mut self, event: DriverEvent) {
        match event {
            DriverEvent::Agent(agent) => {
                let starting = matches!(agent, AgentEvent::OperationStarted { .. });
                if let AgentEvent::OperationSettled {
                    operation_id,
                    status,
                    durability,
                    session_revision,
                } = &agent
                {
                    if let Some(active) = &mut self.active {
                        active.settled_event_seen = true;
                    }
                    let session_id = self
                        .active
                        .as_ref()
                        .map(|active| active.session_id.clone())
                        .or_else(|| self.ledger.session_id(operation_id))
                        .expect("settlement belongs to an admitted operation");
                    let failure = self
                        .active
                        .as_ref()
                        .and_then(|active| active.shared.failure());
                    self.record_settled(
                        operation_id.clone(),
                        session_id.clone(),
                        *status,
                        *durability,
                        failure,
                    );
                    self.release_transients_for(&agent);
                    self.emit(RuntimeEvent::OperationSettled {
                        operation_id: operation_id.clone(),
                        session_id,
                        status: *status,
                        durability: *durability,
                        session_revision: *session_revision,
                    });
                    return;
                }
                self.release_transients_for(&agent);
                self.emit(RuntimeEvent::Agent(agent));
                if starting {
                    if let Some(active) = &mut self.active {
                        active.started = true;
                    }
                }
            }
        }
    }

    fn drain_driver_events(&mut self) {
        while let Some(event) = self
            .driver_events
            .as_mut()
            .and_then(|rx| rx.try_recv().ok())
        {
            self.forward_driver_event(event);
        }
        self.pull_caught_up_transients();
    }

    fn on_driver_join(&mut self, join: Option<Result<DriverExit, tokio::task::JoinError>>) {
        self.drain_driver_events();
        let Some(join) = join else {
            return;
        };
        let exit = match join {
            Ok(exit) => exit,
            Err(error) if error.is_panic() => DriverExit::Panicked {
                diagnostic_id: self.next_diagnostic(),
            },
            Err(_) => DriverExit::Aborted {
                diagnostic_id: self.next_diagnostic(),
            },
        };
        let Some(active) = self.active.take() else {
            self.driver_events = None;
            self.driver_join = None;
            return;
        };
        self.driver_events = None;
        self.driver_join = None;
        if !active.settled_event_seen {
            let (status, durability, fault) = match &exit {
                DriverExit::Succeeded => (
                    OperationStatus::Succeeded,
                    SettlementDurability::Confirmed,
                    None,
                ),
                DriverExit::FailedConfirmed => (
                    OperationStatus::Failed,
                    SettlementDurability::Confirmed,
                    None,
                ),
                DriverExit::FailedUnconfirmed
                | DriverExit::Panicked { .. }
                | DriverExit::Aborted { .. } => (
                    OperationStatus::Failed,
                    SettlementDurability::Unconfirmed,
                    match &exit {
                        DriverExit::Panicked { diagnostic_id }
                        | DriverExit::Aborted { diagnostic_id } => Some((
                            diagnostic_id.clone(),
                            "operation driver ended without a confirmed settlement",
                        )),
                        _ => None,
                    },
                ),
                DriverExit::CancelledConfirmed => (
                    OperationStatus::Cancelled,
                    SettlementDurability::Confirmed,
                    None,
                ),
            };
            self.emit(RuntimeEvent::OperationSettled {
                operation_id: active.operation_id.clone(),
                session_id: active.session_id.clone(),
                status,
                durability,
                session_revision: SettlementRevision::Unchanged,
            });
            self.record_settled(
                active.operation_id.clone(),
                active.session_id.clone(),
                status,
                durability,
                active.shared.failure(),
            );
            if let Some((diagnostic_id, message)) = fault {
                self.emit(RuntimeEvent::RuntimeFault {
                    diagnostic_id,
                    message: message.to_owned(),
                });
            }
        }
        let _ = active;
        self.maybe_start_next();
    }

    fn on_maintenance_join(
        &mut self,
        join: Option<
            Result<Result<crate::CompactionReport, crate::CompactionError>, tokio::task::JoinError>,
        >,
    ) {
        let Some(join) = join else {
            return;
        };
        let Some(maintenance) = self.maintenance.take() else {
            self.maintenance_join = None;
            return;
        };
        self.maintenance_join = None;
        let result = match join {
            Ok(Ok(report)) => {
                if maintenance.cancel.is_requested()
                    && matches!(report, crate::CompactionReport::NothingToCompact)
                {
                    MaintenanceResult::Cancelled
                } else {
                    MaintenanceResult::Compacted(report)
                }
            }
            Ok(Err(error)) => {
                if maintenance.cancel.is_requested() {
                    MaintenanceResult::Cancelled
                } else {
                    MaintenanceResult::Failed(error)
                }
            }
            Err(error) if error.is_panic() => MaintenanceResult::Panicked {
                diagnostic_id: self.next_diagnostic(),
            },
            Err(_) => MaintenanceResult::Cancelled,
        };
        // Cancelled manual compaction that actually committed is still Compacted;
        // cancel before work is Cancelled. compact_manually returns Err only for
        // real failures; a cancel mid-summary maps through compaction.
        let result = if maintenance.cancel.is_requested()
            && matches!(result, MaintenanceResult::Failed(_))
        {
            MaintenanceResult::Cancelled
        } else {
            result
        };
        self.emit(RuntimeEvent::MaintenanceSettled {
            id: maintenance.id,
            session_id: maintenance.session_id,
            result,
        });
        self.maybe_start_next();
    }

    fn on_operation_timeout(&mut self) {
        if let Some(active) = &self.active {
            active.shared.request_cancel(CancelReason::Timeout);
        }
    }

    async fn on_shutdown_timeout(&mut self) {
        self.finish_epoch().await;
    }

    fn begin_shutdown(
        &mut self,
        mode: ShutdownMode,
        reply: Option<oneshot::Sender<ShutdownReport>>,
    ) {
        if matches!(self.shutdown, ShutdownState::Stopped) {
            if let Some(reply) = reply {
                let _ = reply.send(self.shutdown_report());
            }
            return;
        }
        if let Some(reply) = self.shutdown_reply.take() {
            let _ = reply.send(self.shutdown_report());
        }
        self.shutdown_reply = reply;
        match mode {
            ShutdownMode::Drain => {
                self.shutdown = ShutdownState::Draining;
                self.reject_queue();
            }
            ShutdownMode::Forced => {
                self.shutdown = ShutdownState::Forced;
                self.shutdown_deadline = Some(Instant::now() + Duration::from_secs(5));
                self.reject_queue();
                self.force_abort_children();
            }
        }
    }

    fn reject_queue(&mut self) {
        let queued: Vec<_> = self.queue.drain(..).collect();
        for item in queued {
            self.emit(RuntimeEvent::OperationSettled {
                operation_id: item.operation_id.clone(),
                session_id: item.spec.session_id.clone(),
                status: OperationStatus::Failed,
                durability: SettlementDurability::Unconfirmed,
                session_revision: SettlementRevision::Unchanged,
            });
            self.record_settled(
                item.operation_id,
                item.spec.session_id,
                OperationStatus::Failed,
                SettlementDurability::Unconfirmed,
                None,
            );
        }
    }

    fn force_abort_children(&mut self) {
        if let Some(join) = &self.driver_join {
            join.abort();
        }
        if let Some(join) = &self.maintenance_join {
            join.abort();
        }
        if let Some(maintenance) = &self.maintenance {
            maintenance.cancel.request();
        }
        if let Some(active) = &self.active {
            active.shared.request_cancel(CancelReason::User);
        }
    }

    fn maybe_start_next(&mut self) {
        if !matches!(self.shutdown, ShutdownState::Running) {
            return;
        }
        if self.active.is_some() || self.maintenance.is_some() {
            return;
        }
        if let Some(next) = self.queue.pop_front() {
            self.spawn_driver(next);
        }
    }

    async fn finish_epoch(&mut self) {
        if matches!(self.shutdown, ShutdownState::Stopped) {
            return;
        }
        self.force_abort_children();
        self.drain_driver_events();
        self.join_children_with_deadline().await;
        self.drain_driver_events();
        self.flush_reliable_await().await;
        let mut settlements = Vec::new();
        while let Some(entry) = self.ledger.pop() {
            let diagnostic_id = self.next_diagnostic();
            let event = RuntimeEvent::OperationSettled {
                operation_id: entry.operation_id.clone(),
                session_id: entry.session_id.clone(),
                status: OperationStatus::Failed,
                durability: SettlementDurability::Unconfirmed,
                session_revision: SettlementRevision::Unchanged,
            };
            if self.event_tx.send(event).await.is_err() {
                break;
            }
            self.last_settled.push(SettledOperationSnapshot {
                operation_id: entry.operation_id.clone(),
                session_id: entry.session_id.clone(),
                status: OperationStatus::Failed,
                durability: SettlementDurability::Unconfirmed,
                failure: None,
            });
            if self.last_settled.len() > 32 {
                self.last_settled.remove(0);
            }
            settlements.push(ForcedSettlement {
                operation_id: entry.operation_id,
                session_id: entry.session_id,
                status: OperationStatus::Failed,
                durability: SettlementDurability::Unconfirmed,
                diagnostic_id,
            });
        }
        self.active = None;
        self.queue.clear();
        self.driver_events = None;
        if let Some(maintenance) = self.maintenance.take() {
            let _ = self
                .event_tx
                .send(RuntimeEvent::MaintenanceSettled {
                    id: maintenance.id,
                    session_id: maintenance.session_id,
                    result: MaintenanceResult::Cancelled,
                })
                .await;
        }
        self.shutdown = ShutdownState::Stopped;
        let forced_count = settlements.len();
        if self
            .event_tx
            .send(RuntimeEvent::EpochEnded {
                epoch: self.epoch.clone(),
                reason: EpochEndReason::Shutdown,
                forced_count,
            })
            .await
            .is_ok()
        {
            self.epoch_ended.store(true, Ordering::SeqCst);
        }
        self.publish_snapshot();
        self.finished_report = Some(ShutdownReport {
            epoch: self.epoch.clone(),
            final_state: ShutdownState::Stopped,
            settlements,
            diagnostics: Vec::new(),
        });
    }

    async fn join_children_with_deadline(&mut self) {
        if let Some(join) = self.driver_join.take() {
            let _ = abort_and_join(join, EPOCH_CHILD_JOIN_DEADLINE).await;
        }
        if let Some(join) = self.maintenance_join.take() {
            let _ = abort_and_join(join, EPOCH_CHILD_JOIN_DEADLINE).await;
        }
    }

    fn active_deadline(&self) -> Option<Instant> {
        self.active
            .as_ref()
            .and_then(|active| active.shared.deadline())
    }

    fn availability(&self) -> AgentAvailability {
        if let Some(maintenance) = &self.maintenance {
            AgentAvailability::Compacting {
                session_id: maintenance.session_id.clone(),
            }
        } else if let Some(active) = &self.active {
            AgentAvailability::Busy {
                operation_id: active.operation_id.clone(),
            }
        } else {
            AgentAvailability::Idle
        }
    }

    fn record_settled(
        &mut self,
        operation_id: OperationId,
        session_id: SessionId,
        status: OperationStatus,
        durability: SettlementDurability,
        failure: Option<AgentFailure>,
    ) {
        self.last_settled.push(SettledOperationSnapshot {
            operation_id,
            session_id,
            status,
            durability,
            failure,
        });
        if self.last_settled.len() > 32 {
            self.last_settled.remove(0);
        }
        self.publish_snapshot();
    }

    fn emit_availability_if_changed(&mut self) {
        let availability = self.availability();
        let queued = self.queue.len();
        if availability != self.last_availability || queued != self.last_queued_len {
            self.last_availability = availability.clone();
            self.last_queued_len = queued;
            self.emit(RuntimeEvent::AvailabilityChanged {
                availability,
                queued,
            });
        }
    }

    fn publish_snapshot(&mut self) {
        self.runtime_revision = self.runtime_revision.saturating_add(1);
        let snapshot = RuntimeSnapshot {
            epoch: self.epoch.clone(),
            availability: self.availability(),
            queued: self
                .queue
                .iter()
                .map(|queued| QueuedOperationSnapshot {
                    operation_id: queued.operation_id.clone(),
                    session_id: queued.spec.session_id.clone(),
                })
                .collect(),
            active: self.active.as_ref().map(|active| ActiveOperationSnapshot {
                operation_id: active.operation_id.clone(),
                turn_id: active.turn_id.clone(),
                session_id: active.session_id.clone(),
                phase: active.shared.phase(),
                started: active.started,
            }),
            maintenance: self
                .maintenance
                .as_ref()
                .map(|maintenance| MaintenanceSnapshot {
                    id: maintenance.id.clone(),
                    session_id: maintenance.session_id.clone(),
                }),
            shutdown: self.shutdown,
            last_settled: self.last_settled.clone(),
            runtime_revision: self.runtime_revision,
        };
        let _ = self.snapshot_tx.send(snapshot);
    }

    fn shutdown_report(&self) -> ShutdownReport {
        ShutdownReport {
            epoch: self.epoch.clone(),
            final_state: self.shutdown,
            settlements: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn next_diagnostic(&self) -> DiagnosticId {
        DiagnosticId::new(format!(
            "diag-{}",
            self.next_diagnostic.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn can_emit_transient(&self) -> bool {
        self.active.as_ref().is_some_and(|active| active.started)
    }

    fn driver_events_pending(&self) -> bool {
        self.driver_events.as_ref().is_some_and(|rx| !rx.is_empty())
    }

    fn pull_caught_up_transients(&mut self) {
        if !self.driver_events_pending() {
            let events = self.drain_all_transients();
            self.emit_transient_agents(events);
        }
    }

    fn release_transients_for(&mut self, agent: &AgentEvent) {
        if !self.driver_events_pending() {
            let events = self.drain_all_transients();
            self.emit_transient_agents(events);
            return;
        }
        let events = match agent {
            AgentEvent::AssistantMessageCompleted { .. }
            | AgentEvent::TurnFailed { .. }
            | AgentEvent::TurnCancelled { .. }
            | AgentEvent::ToolBatchRequested { .. } => self
                .active
                .as_ref()
                .map(|active| active.shared.drain_model_stream())
                .unwrap_or_default(),
            AgentEvent::ToolExecutionCompleted { .. } => self
                .active
                .as_ref()
                .map(|active| active.shared.drain_tool_progress())
                .unwrap_or_default(),
            AgentEvent::OperationSettled { .. } | AgentEvent::CancellationRequested { .. } => {
                self.drain_all_transients()
            }
            _ => Vec::new(),
        };
        self.emit_transient_agents(events);
    }

    fn drain_all_transients(&mut self) -> Vec<AgentEvent> {
        let Some(active) = &self.active else {
            return Vec::new();
        };
        let (phase, events) = active.shared.drain_transients();
        if let Some(phase) = phase {
            if let Some(active) = &mut self.active {
                active.phase = phase;
            }
        }
        events
    }

    fn emit_transient_agents(&mut self, events: Vec<AgentEvent>) {
        for event in events {
            if matches!(
                event,
                AgentEvent::TextDelta { .. } | AgentEvent::ReasoningDelta { .. }
            ) && self.can_emit_transient()
            {
                if let Some(held) = self.transient_hold.take() {
                    self.pending_reliable.push_back(held);
                }
            }
            self.emit(RuntimeEvent::Agent(event));
        }
    }

    fn emit(&mut self, event: RuntimeEvent) {
        if is_mergeable(&event) {
            if let Some(displaced) = self.transient_hold.publish(event) {
                if self.can_emit_transient() {
                    self.pending_reliable.push_back(displaced);
                }
            }
            return;
        }
        if self.can_emit_transient() && should_release_hold(&event) {
            if let Some(held) = self.transient_hold.take() {
                self.pending_reliable.push_back(held);
            }
        }
        self.pending_reliable.push_back(event);
    }

    fn flush_outbound(&mut self) {
        while let Some(event) = self.pending_reliable.pop_front() {
            let settled = settled_operation_id(&event);
            match self.event_tx.try_send(event) {
                Ok(()) => {
                    if let Some(operation_id) = settled {
                        self.ledger.remove(&operation_id);
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.pending_reliable.clear();
                    return;
                }
                Err(mpsc::error::TrySendError::Full(event)) => {
                    self.pending_reliable.push_front(event);
                    return;
                }
            }
        }
        if self.event_tx.capacity() == self.event_tx.max_capacity()
            && self.can_emit_transient()
            && !self.driver_events_pending()
        {
            if let Some(held) = self.transient_hold.take() {
                match self.event_tx.try_send(held) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => {}
                    Err(mpsc::error::TrySendError::Full(held)) => {
                        self.transient_hold.restore(held);
                    }
                }
            }
        }
    }

    async fn flush_reliable_await(&mut self) {
        while let Some(event) = self.pending_reliable.pop_front() {
            let settled = settled_operation_id(&event);
            if self.event_tx.send(event).await.is_err() {
                self.pending_reliable.clear();
                return;
            }
            if let Some(operation_id) = settled {
                self.ledger.remove(&operation_id);
            }
        }
    }
}

impl Drop for Coordinator {
    fn drop(&mut self) {
        if let Some(active) = &self.active {
            active.shared.request_cancel(CancelReason::User);
        }
        if let Some(maintenance) = &self.maintenance {
            maintenance.cancel.request();
        }
        if let Some(join) = &self.driver_join {
            join.abort();
        }
        if let Some(join) = &self.maintenance_join {
            join.abort();
        }
        self.children.publish(EpochChildren {
            driver: self.driver_join.take(),
            maintenance: self.maintenance_join.take(),
            maintenance_id: self
                .maintenance
                .as_ref()
                .map(|maintenance| maintenance.id.clone()),
            maintenance_session_id: self
                .maintenance
                .as_ref()
                .map(|maintenance| maintenance.session_id.clone()),
        });
    }
}

async fn recv_driver(slot: &mut Option<mpsc::Receiver<DriverEvent>>) -> Option<DriverEvent> {
    match slot {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn join_driver(
    slot: &mut Option<JoinHandle<DriverExit>>,
) -> Option<Result<DriverExit, tokio::task::JoinError>> {
    match slot {
        Some(join) => Some((&mut *join).await),
        None => std::future::pending().await,
    }
}

async fn join_maintenance(
    slot: &mut Option<JoinHandle<Result<crate::CompactionReport, crate::CompactionError>>>,
) -> Option<Result<Result<crate::CompactionReport, crate::CompactionError>, tokio::task::JoinError>>
{
    match slot {
        Some(join) => Some((&mut *join).await),
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep(at.saturating_duration_since(Instant::now())).await,
        None => std::future::pending().await,
    }
}

pub(crate) fn empty_snapshot(epoch: RuntimeEpoch) -> RuntimeSnapshot {
    RuntimeSnapshot {
        epoch,
        availability: AgentAvailability::Idle,
        queued: Vec::new(),
        active: None,
        maintenance: None,
        shutdown: ShutdownState::Running,
        last_settled: Vec::new(),
        runtime_revision: 0,
    }
}

fn settled_operation_id(event: &RuntimeEvent) -> Option<OperationId> {
    match event {
        RuntimeEvent::OperationSettled { operation_id, .. } => Some(operation_id.clone()),
        _ => None,
    }
}

fn is_prefix_agent(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::OperationStarted { .. }
            | AgentEvent::PriorTurnSealed { .. }
            | AgentEvent::ContextCompactionStarted
            | AgentEvent::ContextCompactionCompleted { .. }
            | AgentEvent::ContextCompactionFailed { .. }
            | AgentEvent::TurnStarted { .. }
            | AgentEvent::ModelCallStarted { .. }
            | AgentEvent::ModelResponseStarted { .. }
            | AgentEvent::ToolExecutionStarted { .. }
    )
}

fn should_release_hold(event: &RuntimeEvent) -> bool {
    match event {
        RuntimeEvent::Agent(agent) => !is_prefix_agent(agent),
        _ => true,
    }
}

async fn wait_driver_transients(active: &Option<ActiveMeta>, wait: bool) {
    if !wait {
        std::future::pending::<()>().await;
        return;
    }
    match active {
        Some(active) => active.shared.wait_transients().await,
        None => std::future::pending().await,
    }
}

async fn reserve_if_pending(
    tx: &mpsc::Sender<RuntimeEvent>,
    has_pending: bool,
) -> Option<Result<mpsc::Permit<'_, RuntimeEvent>, mpsc::error::SendError<()>>> {
    if !has_pending {
        std::future::pending::<()>().await;
        None
    } else {
        Some(tx.reserve().await)
    }
}
