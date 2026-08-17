//! Epoch-root supervision: accepted ledger, child handoff, and panic recovery.

use crate::{
    AgentAvailability, AgentEvent, DiagnosticId, EpochSettlement, MaintenanceId, MaintenanceResult,
    OperationId, OperationStatus, RuntimeEpoch, RuntimeEvent, RuntimeSnapshot,
    SettlementDurability, ShutdownState, TurnId,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

pub(crate) const EPOCH_CHILD_JOIN_DEADLINE: Duration = Duration::from_secs(5);

/// Shared epoch state handed to the coordinator and the root supervisor.
#[derive(Clone)]
pub(crate) struct EpochShared {
    pub ledger: AcceptedLedger,
    pub children: EpochChildHandoff,
    pub epoch_ended: Arc<AtomicBool>,
}

impl EpochShared {
    pub(crate) fn new(queue_max: usize) -> Self {
        Self {
            ledger: AcceptedLedger::new(queue_max.saturating_add(1)),
            children: EpochChildHandoff::new(),
            epoch_ended: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn mark_epoch_ended(&self) {
        self.epoch_ended.store(true, Ordering::SeqCst);
    }

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
    states: HashMap<OperationId, TurnId>,
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

    pub(crate) fn insert(&self, operation_id: OperationId, turn_id: TurnId) -> Result<(), ()> {
        let mut inner = lock(&self.inner);
        if inner.states.contains_key(&operation_id) {
            return Ok(());
        }
        if inner.order.len() >= inner.cap {
            return Err(());
        }
        inner.order.push(operation_id.clone());
        inner.states.insert(operation_id, turn_id);
        Ok(())
    }

    pub(crate) fn remove(&self, operation_id: &OperationId) -> bool {
        let mut inner = lock(&self.inner);
        let existed = inner.states.remove(operation_id).is_some();
        if existed {
            inner.order.retain(|id| id != operation_id);
        }
        existed
    }

    pub(crate) fn take_all(&self) -> Vec<(OperationId, TurnId)> {
        let mut inner = lock(&self.inner);
        let order = std::mem::take(&mut inner.order);
        let mut states = std::mem::take(&mut inner.states);
        order
            .into_iter()
            .filter_map(|id| states.remove(&id).map(|turn_id| (id, turn_id)))
            .collect()
    }

    pub(crate) fn pop(&self) -> Option<(OperationId, TurnId)> {
        let mut inner = lock(&self.inner);
        let id = inner.order.first().cloned()?;
        inner.order.remove(0);
        inner.states.remove(&id).map(|turn_id| (id, turn_id))
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

pub(crate) struct SuperviseEpoch {
    pub epoch: RuntimeEpoch,
    pub coordinator: JoinHandle<()>,
    pub shared: EpochShared,
    pub event_tx: tokio::sync::mpsc::Sender<RuntimeEvent>,
    pub snapshot_tx: tokio::sync::watch::Sender<RuntimeSnapshot>,
}

pub(crate) async fn supervise_epoch(input: SuperviseEpoch) {
    let outcome = input.coordinator.await;
    let children = input.shared.children.take();
    let maintenance_id = children.maintenance_id.clone();
    let leftover = input.shared.ledger.take_all();
    let join_faults = join_epoch_children(children).await;
    let panicked = outcome.is_err();
    if input.shared.epoch_ended() {
        return;
    }
    if panicked || !leftover.is_empty() {
        input.shared.mark_epoch_ended();
        publish_forced_end(
            input.event_tx,
            input.snapshot_tx,
            input.epoch,
            leftover,
            maintenance_id.filter(|_| panicked),
            join_faults,
        )
        .await;
    }
}

async fn publish_forced_end(
    event_tx: tokio::sync::mpsc::Sender<RuntimeEvent>,
    snapshot_tx: tokio::sync::watch::Sender<RuntimeSnapshot>,
    epoch: RuntimeEpoch,
    leftover: Vec<(OperationId, TurnId)>,
    maintenance_id: Option<MaintenanceId>,
    join_faults: Vec<String>,
) {
    let mut settlements = Vec::new();
    let mut last_settled = snapshot_tx.borrow().last_settled.clone();
    if let Some(id) = maintenance_id {
        let _ = event_tx
            .send(RuntimeEvent::MaintenanceSettled {
                id,
                result: MaintenanceResult::Cancelled,
            })
            .await;
    }
    for (index, (operation_id, _)) in leftover.into_iter().enumerate() {
        let diagnostic_id =
            DiagnosticId::new(format!("epoch-{}-forced-{}", epoch.as_str(), index + 1));
        let _ = event_tx
            .send(RuntimeEvent::Agent(AgentEvent::OperationSettled {
                operation_id: operation_id.clone(),
                status: OperationStatus::Failed,
                durability: SettlementDurability::Unconfirmed,
                session_revision: None,
            }))
            .await;
        last_settled.push(crate::SettledOperationSnapshot {
            operation_id: operation_id.clone(),
            status: OperationStatus::Failed,
            durability: SettlementDurability::Unconfirmed,
            failure: Some(crate::AgentFailure::runtime_driver(
                "runtime epoch ended before the operation settled",
            )),
        });
        if last_settled.len() > 32 {
            last_settled.remove(0);
        }
        settlements.push(EpochSettlement {
            operation_id,
            status: OperationStatus::Failed,
            durability: SettlementDurability::Unconfirmed,
            diagnostic_id,
        });
    }
    for (index, message) in join_faults.into_iter().enumerate() {
        let _ = event_tx
            .send(RuntimeEvent::RuntimeFault {
                diagnostic_id: DiagnosticId::new(format!(
                    "epoch-{}-child-{}",
                    epoch.as_str(),
                    index + 1
                )),
                message,
            })
            .await;
    }
    let _ = event_tx
        .send(RuntimeEvent::EpochEnded {
            epoch: epoch.clone(),
            settlements,
        })
        .await;
    let mut snapshot = snapshot_tx.borrow().clone();
    snapshot.epoch = epoch;
    snapshot.availability = AgentAvailability::Idle;
    snapshot.queued.clear();
    snapshot.active = None;
    snapshot.maintenance = None;
    snapshot.shutdown = ShutdownState::Stopped;
    snapshot.last_settled = last_settled;
    snapshot.runtime_revision = snapshot.runtime_revision.saturating_add(1);
    let _ = snapshot_tx.send(snapshot);
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
