//! State shared between a driver task and the coordinator.

use crate::error::DriverExit;
use crate::transient::{TransientDriverState, is_transient_agent};
use crate::{
    AgentEvent, AgentFailure, DiagnosticId, OperationId, OperationOutcome, OperationPhase,
    OperationStatus, SettlementRevision, TokenUsage, TurnId,
};
use philo_session::CancelReason;
use philo_tools::ToolCancel;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};

/// One reliable driver-to-coordinator notification.
pub(crate) enum DriverEvent {
    Agent(AgentEvent),
}

/// Explicit cancel flag for a maintenance task. Dropping a handle never
/// cancels; the coordinator must call [`MaintenanceCancel::request`].
pub(crate) struct MaintenanceCancel {
    requested: AtomicBool,
    notify: Notify,
}

impl MaintenanceCancel {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            requested: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    pub(crate) fn request(&self) {
        self.requested.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub(crate) fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    pub(crate) async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_requested() {
                return;
            }
            notified.await;
        }
    }
}

/// State shared between the coordinator and one spawned driver.
pub(crate) struct OperationShared {
    pub(super) operation_id: OperationId,
    pub(super) turn_id: TurnId,
    cancel_requested: AtomicBool,
    tool_cancel: ToolCancel,
    cancel_reason: Mutex<Option<CancelReason>>,
    deadline: Mutex<Option<Instant>>,
    notify: Notify,
    phase: Mutex<OperationPhase>,
    exit: Mutex<Option<DriverExit>>,
    failure: Mutex<Option<AgentFailure>>,
    events: mpsc::Sender<DriverEvent>,
    transient: TransientDriverState,
    settlement_revision: Mutex<Option<SettlementRevision>>,
    /// Latest token usage observed for this operation's model call(s).
    /// Captured from `ModelUsageUpdated` before the transient store
    /// drains it, so settlement entries can persist the value.
    last_usage: Mutex<Option<TokenUsage>>,
}

impl OperationShared {
    pub(crate) fn new(
        operation_id: OperationId,
        turn_id: TurnId,
        events: mpsc::Sender<DriverEvent>,
        phase: OperationPhase,
    ) -> Self {
        Self {
            operation_id,
            turn_id,
            cancel_requested: AtomicBool::new(false),
            tool_cancel: ToolCancel::new(),
            cancel_reason: Mutex::new(None),
            deadline: Mutex::new(None),
            notify: Notify::new(),
            phase: Mutex::new(phase),
            exit: Mutex::new(None),
            failure: Mutex::new(None),
            events,
            transient: TransientDriverState::new(),
            settlement_revision: Mutex::new(None),
            last_usage: Mutex::new(None),
        }
    }

    pub(crate) fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }
    pub(crate) fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    pub(crate) async fn publish(&self, event: AgentEvent) {
        if is_transient_agent(&event) {
            if let AgentEvent::ModelUsageUpdated { usage, .. } = &event {
                *lock(&self.last_usage) = Some(*usage);
            }
            self.transient.publish_agent(event);
            return;
        }
        if let AgentEvent::OperationSettled {
            session_revision, ..
        } = &event
        {
            *lock(&self.settlement_revision) = Some(*session_revision);
        }
        self.transient.seal_model_stream();
        let _ = self.events.send(DriverEvent::Agent(event)).await;
    }

    pub(crate) fn publish_tool_progress(&self, event: AgentEvent) {
        self.transient.publish_agent(event);
    }

    pub(crate) fn set_phase(&self, phase: OperationPhase) {
        *lock(&self.phase) = phase.clone();
        self.transient.publish_phase(phase);
    }

    pub(crate) fn drain_transients(&self) -> (Option<OperationPhase>, Vec<AgentEvent>) {
        self.transient.drain()
    }

    pub(crate) fn drain_model_stream(&self) -> Vec<AgentEvent> {
        self.transient.drain_model_stream()
    }

    pub(crate) fn drain_tool_progress(&self) -> Vec<AgentEvent> {
        self.transient.drain_tool_progress()
    }

    pub(crate) async fn wait_transients(&self) {
        self.transient.wait().await;
    }

    pub(crate) fn phase(&self) -> OperationPhase {
        lock(&self.phase).clone()
    }

    pub(crate) fn request_cancel(&self, reason: CancelReason) {
        let mut slot = lock(&self.cancel_reason);
        if slot.is_none() {
            *slot = Some(reason);
            self.cancel_requested.store(true, Ordering::SeqCst);
            self.tool_cancel.request();
            self.notify.notify_waiters();
        }
    }

    pub(crate) fn tool_cancel(&self) -> ToolCancel {
        self.tool_cancel.clone()
    }

    pub(crate) fn cancel_reason(&self) -> CancelReason {
        lock(&self.cancel_reason).unwrap_or(CancelReason::User)
    }

    pub(crate) fn arm_deadline(&self, timeout: Option<Duration>) {
        *lock(&self.deadline) = timeout.map(|timeout| Instant::now() + timeout);
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        *lock(&self.deadline)
    }

    pub(crate) fn is_cancel_requested(&self) -> bool {
        if !self.cancel_requested.load(Ordering::SeqCst)
            && lock(&self.deadline).is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.request_cancel(CancelReason::Timeout);
        }
        self.cancel_requested.load(Ordering::SeqCst)
    }

    pub(crate) async fn wait_until_cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_cancel_requested() {
                return;
            }
            if let Some(deadline) = self.deadline() {
                let wait = deadline.saturating_duration_since(Instant::now());
                tokio::select! {
                    _ = notified => {}
                    _ = tokio::time::sleep(wait) => {
                        self.request_cancel(CancelReason::Timeout);
                        return;
                    }
                }
            } else {
                notified.await;
            }
        }
    }

    /// Transitions to `Settled` once. A second call is a no-op and returns `false`.
    pub(crate) fn settle(&self, outcome: OperationOutcome) -> bool {
        let mut phase = lock(&self.phase);
        if matches!(*phase, OperationPhase::Settled(_)) {
            return false;
        }
        let status = match &outcome {
            OperationOutcome::Succeeded { .. } => OperationStatus::Succeeded,
            OperationOutcome::Failed { .. } => OperationStatus::Failed,
            OperationOutcome::Cancelled => OperationStatus::Cancelled,
        };
        *phase = OperationPhase::Settled(status);
        drop(phase);
        if let OperationOutcome::Failed { failure, .. } = &outcome {
            *lock(&self.failure) = Some(failure.clone());
        }
        *lock(&self.exit) = Some(match &outcome {
            OperationOutcome::Succeeded { .. } => DriverExit::Succeeded,
            OperationOutcome::Failed {
                durability: crate::SettlementDurability::Confirmed,
                ..
            } => DriverExit::FailedConfirmed,
            OperationOutcome::Failed {
                durability: crate::SettlementDurability::Unconfirmed,
                ..
            } => DriverExit::FailedUnconfirmed,
            OperationOutcome::Cancelled => DriverExit::CancelledConfirmed,
        });
        true
    }

    pub(crate) fn failure(&self) -> Option<AgentFailure> {
        lock(&self.failure).clone()
    }

    pub(crate) fn settlement_revision(&self) -> Option<SettlementRevision> {
        *lock(&self.settlement_revision)
    }

    /// Returns the latest token usage observed for this operation, if any.
    /// Does not clear the slot; settlement reads it once.
    pub(crate) fn last_usage(&self) -> Option<TokenUsage> {
        *lock(&self.last_usage)
    }

    pub(crate) fn has_committed_settlement(&self) -> bool {
        matches!(self.phase(), OperationPhase::Settled(_))
            && matches!(
                self.settlement_revision(),
                Some(SettlementRevision::Committed(_))
            )
    }

    pub(crate) fn sealed_model_stream_stats(&self) -> (usize, usize) {
        (self.transient.sealed_len(), self.transient.sealed_cap())
    }

    pub(crate) fn take_exit(&self) -> DriverExit {
        lock(&self.exit).take().unwrap_or(DriverExit::Aborted {
            diagnostic_id: DiagnosticId::new("driver-missing-exit"),
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AssistantMessage, OperationId, TurnId};

    fn shared() -> OperationShared {
        let (tx, _rx) = mpsc::channel(1);
        OperationShared::new(
            OperationId::new("op-1"),
            TurnId::new("turn-1"),
            tx,
            OperationPhase::PreparingTurn,
        )
    }

    #[test]
    fn settle_is_idempotent() {
        let shared = shared();
        assert!(shared.settle(OperationOutcome::Cancelled));
        assert_eq!(
            shared.phase(),
            OperationPhase::Settled(OperationStatus::Cancelled)
        );
        assert!(!shared.settle(OperationOutcome::Succeeded {
            assistant: AssistantMessage {
                content: "late".into(),
            },
        }));
        assert_eq!(
            shared.phase(),
            OperationPhase::Settled(OperationStatus::Cancelled)
        );
        assert_eq!(shared.take_exit(), DriverExit::CancelledConfirmed);
    }

    #[tokio::test]
    async fn publish_records_committed_settlement_revision() {
        let (tx, mut rx) = mpsc::channel(1);
        let shared = OperationShared::new(
            OperationId::new("op-1"),
            TurnId::new("turn-1"),
            tx,
            OperationPhase::PreparingTurn,
        );
        assert!(shared.settle(OperationOutcome::Cancelled));
        shared
            .publish(AgentEvent::OperationSettled {
                operation_id: OperationId::new("op-1"),
                status: OperationStatus::Cancelled,
                durability: crate::SettlementDurability::Confirmed,
                session_revision: SettlementRevision::Committed(
                    philo_session::SessionRevision::new(7),
                ),
            })
            .await;
        assert!(shared.has_committed_settlement());
        assert!(matches!(
            shared.settlement_revision(),
            Some(SettlementRevision::Committed(revision)) if revision.get() == 7
        ));
        assert!(rx.recv().await.is_some());
    }
}
