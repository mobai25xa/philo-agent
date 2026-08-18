//! Epoch-root supervision: accepted ledger, child handoff, and panic recovery.

use crate::shutdown::{ShutdownOutcome, ShutdownRequest, default_shutdown_deadline};
use crate::staging::ReliableStaging;
use crate::{
    AgentAvailability, DiagnosticId, EpochEndReason, ForcedSettlement, MaintenanceId,
    MaintenanceResult, OperationId, OperationStatus, RuntimeEpoch, RuntimeEvent, RuntimeSnapshot,
    SessionId, SettlementDurability, SettlementRevision, ShutdownDiagnostic, ShutdownError,
    ShutdownReport, ShutdownState, TurnId,
};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub(crate) const EPOCH_CHILD_JOIN_DEADLINE: Duration = Duration::from_secs(5);

/// Shared epoch state handed to the coordinator and the root supervisor.
#[derive(Clone)]
pub(crate) struct EpochShared {
    pub ledger: AcceptedLedger,
    pub staging: ReliableStaging,
    pub children: EpochChildHandoff,
    pub epoch_ended: Arc<AtomicBool>,
}

impl EpochShared {
    pub(crate) fn new(queue_max: usize, staging_cap: usize) -> Self {
        Self {
            ledger: AcceptedLedger::new(queue_max.saturating_add(1)),
            staging: ReliableStaging::new(staging_cap),
            children: EpochChildHandoff::new(),
            epoch_ended: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn mark_epoch_ended(&self) {
        self.epoch_ended.store(true, Ordering::SeqCst);
    }

    #[allow(dead_code)]
    pub(crate) fn epoch_ended(&self) -> bool {
        self.epoch_ended.load(Ordering::SeqCst)
    }
}

/// Bounded map of operations accepted in this epoch and not yet settled.
#[derive(Clone)]
pub(crate) struct AcceptedLedger {
    inner: Arc<Mutex<LedgerInner>>,
}

struct LedgerInner {
    cap: usize,
    order: Vec<OperationId>,
    states: HashMap<OperationId, LedgerEntry>,
}

#[derive(Clone, Debug)]
pub(crate) struct LedgerEntry {
    pub operation_id: OperationId,
    #[allow(dead_code)]
    pub turn_id: TurnId,
    pub session_id: SessionId,
}

impl AcceptedLedger {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LedgerInner {
                cap,
                order: Vec::new(),
                states: HashMap::new(),
            })),
        }
    }

    pub(crate) fn insert(
        &self,
        operation_id: OperationId,
        turn_id: TurnId,
        session_id: SessionId,
    ) -> Result<(), ()> {
        let mut inner = lock(&self.inner);
        if inner.states.contains_key(&operation_id) {
            return Ok(());
        }
        if inner.order.len() >= inner.cap {
            return Err(());
        }
        inner.order.push(operation_id.clone());
        inner.states.insert(
            operation_id.clone(),
            LedgerEntry {
                operation_id,
                turn_id,
                session_id,
            },
        );
        Ok(())
    }

    pub(crate) fn session_id(&self, operation_id: &OperationId) -> Option<SessionId> {
        lock(&self.inner)
            .states
            .get(operation_id)
            .map(|entry| entry.session_id.clone())
    }

    pub(crate) fn remove(&self, operation_id: &OperationId) -> bool {
        let mut inner = lock(&self.inner);
        let existed = inner.states.remove(operation_id).is_some();
        if existed {
            inner.order.retain(|id| id != operation_id);
        }
        existed
    }

    pub(crate) fn take_all(&self) -> Vec<LedgerEntry> {
        let mut inner = lock(&self.inner);
        let order = std::mem::take(&mut inner.order);
        let mut states = std::mem::take(&mut inner.states);
        order
            .into_iter()
            .filter_map(|id| states.remove(&id))
            .collect()
    }

    #[allow(dead_code)]
    pub(crate) fn pop(&self) -> Option<LedgerEntry> {
        let mut inner = lock(&self.inner);
        let id = inner.order.first().cloned()?;
        inner.order.remove(0);
        inner.states.remove(&id)
    }
}

/// Driver / maintenance handles transferred to the supervisor on coordinator drop.
#[derive(Clone)]
pub(crate) struct EpochChildHandoff {
    inner: Arc<Mutex<EpochChildren>>,
}

#[derive(Default)]
pub(crate) struct EpochChildren {
    pub driver: Option<JoinHandle<crate::DriverExit>>,
    pub maintenance: Option<JoinHandle<Result<crate::CompactionReport, crate::CompactionError>>>,
    pub maintenance_id: Option<MaintenanceId>,
    pub maintenance_session_id: Option<SessionId>,
}

impl EpochChildHandoff {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(EpochChildren::default())),
        }
    }

    pub(crate) fn publish(&self, children: EpochChildren) {
        *lock(&self.inner) = children;
    }

    pub(crate) fn take(&self) -> EpochChildren {
        std::mem::take(&mut *lock(&self.inner))
    }
}

impl Drop for EpochChildHandoff {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            let children = self.take();
            if let Some(join) = children.driver {
                join.abort();
            }
            if let Some(join) = children.maintenance {
                join.abort();
            }
        }
    }
}

pub(crate) enum ChildJoinOutcome {
    Completed,
    Aborted,
    Panicked,
    DeadlineExceeded,
}

pub(crate) async fn abort_and_join<T: Send + 'static>(
    join: JoinHandle<T>,
    deadline: Duration,
) -> ChildJoinOutcome {
    join.abort();
    match tokio::time::timeout(deadline, join).await {
        Ok(Ok(_)) => ChildJoinOutcome::Completed,
        Ok(Err(error)) if error.is_panic() => ChildJoinOutcome::Panicked,
        Ok(Err(_)) => ChildJoinOutcome::Aborted,
        Err(_) => ChildJoinOutcome::DeadlineExceeded,
    }
}

pub(crate) async fn join_epoch_children(children: EpochChildren) -> Vec<String> {
    let mut forced = Vec::new();
    if let Some(join) = children.driver {
        match abort_and_join(join, EPOCH_CHILD_JOIN_DEADLINE).await {
            ChildJoinOutcome::DeadlineExceeded => {
                forced.push("operation driver exceeded epoch join deadline".to_owned());
            }
            _ => {}
        }
    }
    if let Some(join) = children.maintenance {
        match abort_and_join(join, EPOCH_CHILD_JOIN_DEADLINE).await {
            ChildJoinOutcome::DeadlineExceeded => {
                forced.push("maintenance driver exceeded epoch join deadline".to_owned());
            }
            _ => {}
        }
    }
    forced
}

pub(crate) struct EpochExit {
    pub reason: EpochEndReason,
    pub diagnostics: Vec<ShutdownDiagnostic>,
    pub maintenance: Option<(MaintenanceId, SessionId)>,
}

pub(crate) enum CoordinatorExit {
    Finalized(EpochExit),
}

pub(crate) struct SuperviseEpoch {
    pub epoch: RuntimeEpoch,
    pub coordinator: JoinHandle<CoordinatorExit>,
    pub shared: EpochShared,
    pub event_tx: tokio::sync::mpsc::Sender<RuntimeEvent>,
    pub snapshot_tx: tokio::sync::watch::Sender<RuntimeSnapshot>,
    pub shutdown_rx: watch::Receiver<Option<ShutdownRequest>>,
    pub completion_tx: watch::Sender<Option<ShutdownOutcome>>,
}

pub(crate) async fn supervise_epoch(input: SuperviseEpoch) {
    let outcome = input.coordinator.await;
    let children = input.shared.children.take();
    let children_maintenance = match (
        children.maintenance_id.clone(),
        children.maintenance_session_id.clone(),
    ) {
        (Some(id), Some(session_id)) => Some((id, session_id)),
        _ => None,
    };
    let join_faults = join_epoch_children(children).await;
    let mut exit = match outcome {
        Ok(CoordinatorExit::Finalized(exit)) => exit,
        Err(_) => EpochExit {
            reason: EpochEndReason::CoordinatorFault,
            diagnostics: Vec::new(),
            maintenance: children_maintenance.clone(),
        },
    };
    if exit.maintenance.is_none() {
        exit.maintenance = children_maintenance;
    }
    exit.diagnostics.extend(
        join_faults
            .into_iter()
            .map(|message| ShutdownDiagnostic { message }),
    );
    let staged = input.shared.staging.drain();
    let leftover = input.shared.ledger.take_all();
    // Leftover forced settlements only cover ledger ops that never reached
    // staging as OperationSettled. Already staged terminals are not repeated.
    let settled: HashSet<OperationId> = staged.iter().filter_map(settled_operation_id).collect();
    let forced: Vec<ForcedSettlement> = leftover
        .into_iter()
        .filter(|entry| !settled.contains(&entry.operation_id))
        .enumerate()
        .map(|(index, entry)| ForcedSettlement {
            operation_id: entry.operation_id,
            session_id: entry.session_id,
            status: OperationStatus::Failed,
            durability: SettlementDurability::Unconfirmed,
            diagnostic_id: DiagnosticId::new(format!(
                "epoch-{}-forced-{}",
                input.epoch.as_str(),
                index + 1
            )),
        })
        .collect();
    let deadline = input
        .shutdown_rx
        .borrow()
        .map(|request| request.deadline)
        .unwrap_or_else(default_shutdown_deadline);
    input.shared.mark_epoch_ended();
    let result = apply_epoch_exit(
        input.event_tx,
        input.snapshot_tx,
        input.epoch,
        exit,
        staged,
        forced,
        deadline,
    )
    .await;
    let _ = input.completion_tx.send(Some(result));
}

fn settled_operation_id(event: &RuntimeEvent) -> Option<OperationId> {
    match event {
        RuntimeEvent::OperationSettled { operation_id, .. } => Some(operation_id.clone()),
        _ => None,
    }
}

async fn apply_epoch_exit(
    event_tx: tokio::sync::mpsc::Sender<RuntimeEvent>,
    snapshot_tx: tokio::sync::watch::Sender<RuntimeSnapshot>,
    epoch: RuntimeEpoch,
    exit: EpochExit,
    staged: Vec<RuntimeEvent>,
    forced: Vec<ForcedSettlement>,
    deadline: Instant,
) -> ShutdownOutcome {
    let sink_closed = event_tx.is_closed();
    let mut last_settled = snapshot_tx.borrow().last_settled.clone();
    let mut pending = Vec::new();
    let mut events = staged;
    if let Some((id, session_id)) = exit.maintenance {
        events.push(RuntimeEvent::MaintenanceSettled {
            id,
            session_id,
            result: MaintenanceResult::Cancelled,
        });
    }
    for settlement in &forced {
        events.push(RuntimeEvent::OperationSettled {
            operation_id: settlement.operation_id.clone(),
            session_id: settlement.session_id.clone(),
            status: settlement.status,
            durability: settlement.durability,
            session_revision: SettlementRevision::Unchanged,
        });
        last_settled.push(crate::SettledOperationSnapshot {
            operation_id: settlement.operation_id.clone(),
            session_id: settlement.session_id.clone(),
            status: settlement.status,
            durability: settlement.durability,
            failure: Some(crate::AgentFailure::runtime_driver(
                "runtime epoch ended before the operation settled",
            )),
        });
        if last_settled.len() > 32 {
            last_settled.remove(0);
        }
    }
    for (index, diagnostic) in exit.diagnostics.iter().enumerate() {
        events.push(RuntimeEvent::RuntimeFault {
            diagnostic_id: DiagnosticId::new(format!(
                "epoch-{}-child-{}",
                epoch.as_str(),
                index + 1
            )),
            message: diagnostic.message.clone(),
        });
    }
    let forced_count = forced.len();
    events.push(RuntimeEvent::EpochEnded {
        epoch: epoch.clone(),
        reason: exit.reason,
        forced_count,
    });

    let mut published_epoch_ended = sink_closed;
    if !sink_closed {
        for event in events {
            let is_epoch_ended = matches!(event, RuntimeEvent::EpochEnded { .. });
            match send_terminal(&event_tx, event, deadline).await {
                Ok(()) => {
                    if is_epoch_ended {
                        published_epoch_ended = true;
                    }
                }
                Err(TerminalSendError::Closed) => {
                    published_epoch_ended = true;
                    break;
                }
                Err(TerminalSendError::Timeout) => {
                    pending.push("runtime-event-outlet".to_owned());
                    break;
                }
            }
        }
    }

    let complete = published_epoch_ended || sink_closed;
    let mut snapshot = snapshot_tx.borrow().clone();
    snapshot.epoch = epoch.clone();
    snapshot.availability = AgentAvailability::Idle;
    snapshot.queued.clear();
    snapshot.active = None;
    snapshot.maintenance = None;
    snapshot.shutdown = if complete {
        ShutdownState::Stopped
    } else {
        ShutdownState::Forced
    };
    snapshot.last_settled = last_settled;
    snapshot.runtime_revision = snapshot.runtime_revision.saturating_add(1);
    let _ = snapshot_tx.send(snapshot);

    if complete {
        Ok(ShutdownReport {
            epoch,
            final_state: ShutdownState::Stopped,
            settlements: forced,
            diagnostics: exit.diagnostics,
        })
    } else {
        Err(ShutdownError::DeadlineExceeded { pending })
    }
}

enum TerminalSendError {
    Closed,
    Timeout,
}

async fn send_terminal(
    tx: &tokio::sync::mpsc::Sender<RuntimeEvent>,
    event: RuntimeEvent,
    deadline: Instant,
) -> Result<(), TerminalSendError> {
    if tx.is_closed() {
        return Err(TerminalSendError::Closed);
    }
    if Instant::now() >= deadline {
        return Err(TerminalSendError::Timeout);
    }
    tokio::select! {
        result = tx.send(event) => result.map_err(|_| TerminalSendError::Closed),
        _ = tokio::time::sleep(deadline.saturating_duration_since(Instant::now())) => {
            Err(TerminalSendError::Timeout)
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
