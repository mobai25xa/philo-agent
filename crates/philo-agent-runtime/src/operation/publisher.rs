//! Event publication and one-shot settlement of a driven operation.

use super::shared::OperationShared;
use crate::{
    AgentEvent, AgentFailure, AssistantMessage, OperationId, OperationOutcome, OperationPhase,
    OperationStatus, SettlementDurability, TurnId,
};
use philo_session::CancelReason;
use std::sync::Arc;
use std::time::Duration;

/// Publishes events and phase transitions while an operation is driven and
/// settles the shared state exactly once. Publication is immediate (M10):
/// the barrier-ordering guarantee lives at the call sites, which only push
/// after the corresponding barrier committed.
pub(crate) struct OperationPublisher {
    shared: Arc<OperationShared>,
}

impl OperationPublisher {
    /// Starts driving: publishes `OperationStarted` and arms the automatic
    /// cancellation deadline. `TurnStarted` follows separately through
    /// [`OperationPublisher::turn_started`] so seal notifications (M11) can
    /// land between the two.
    pub fn begin(shared: Arc<OperationShared>, operation_timeout: Option<Duration>) -> Self {
        shared.arm_deadline(operation_timeout);
        shared.publish(AgentEvent::OperationStarted {
            operation_id: shared.operation_id().clone(),
        });
        shared.set_phase(OperationPhase::PreparingTurn);
        Self { shared }
    }

    /// Publishes `TurnStarted` once every stale prior turn is sealed.
    pub fn turn_started(&self) {
        self.shared.publish(AgentEvent::TurnStarted {
            turn_id: self.shared.turn_id().clone(),
        });
    }

    /// Publishes `PriorTurnSealed` after one seal transaction committed.
    pub fn prior_turn_sealed(&self, turn_id: TurnId) {
        self.shared.publish(AgentEvent::PriorTurnSealed { turn_id });
    }

    pub fn operation_id(&self) -> &OperationId {
        self.shared.operation_id()
    }
    pub fn turn_id(&self) -> &TurnId {
        self.shared.turn_id()
    }

    /// The accepted cancellation reason recorded on this operation.
    pub fn cancel_reason(&self) -> CancelReason {
        self.shared.cancel_reason()
    }

    /// Observes a pending cancel request at an injection point (and lazily
    /// promotes an expired deadline into a timeout cancel).
    pub fn is_cancel_requested(&self) -> bool {
        self.shared.is_cancel_requested()
    }

    /// The shared state, for stream polling that must observe cancellation.
    pub(crate) fn shared(&self) -> &OperationShared {
        &self.shared
    }

    pub(crate) fn shared_arc(&self) -> Arc<OperationShared> {
        Arc::clone(&self.shared)
    }

    pub fn push(&self, event: AgentEvent) {
        self.shared.publish(event);
    }

    pub fn set_phase(&self, phase: OperationPhase) {
        self.shared.set_phase(phase);
    }

    /// Publishes `CancellationRequested` when a cancel signal is observed at
    /// an injection point.
    pub fn cancellation_observed(&self) {
        self.shared.publish(AgentEvent::CancellationRequested {
            operation_id: self.shared.operation_id().clone(),
            reason: self.shared.cancel_reason(),
        });
    }

    pub fn succeed(self, assistant: AssistantMessage) {
        self.shared.publish(AgentEvent::AssistantMessageCompleted {
            turn_id: self.shared.turn_id().clone(),
            message: assistant.clone(),
        });
        self.shared.publish(AgentEvent::OperationSettled {
            operation_id: self.shared.operation_id().clone(),
            status: OperationStatus::Succeeded,
            durability: SettlementDurability::Confirmed,
        });
        self.shared
            .settle(OperationOutcome::Succeeded { assistant });
    }

    pub fn fail_confirmed(self, failure: AgentFailure) {
        self.shared.publish(AgentEvent::TurnFailed {
            turn_id: self.shared.turn_id().clone(),
            failure: failure.clone(),
        });
        self.fail(failure, SettlementDurability::Confirmed);
    }

    pub fn fail_unconfirmed(self, failure: AgentFailure) {
        self.fail(failure, SettlementDurability::Unconfirmed);
    }

    fn fail(self, failure: AgentFailure, durability: SettlementDurability) {
        self.shared.publish(AgentEvent::OperationSettled {
            operation_id: self.shared.operation_id().clone(),
            status: OperationStatus::Failed,
            durability,
        });
        self.shared.settle(OperationOutcome::Failed {
            failure,
            durability,
        });
    }

    /// Settles as cancelled before any turn fact was persisted: no
    /// `TurnCancelled` because durably no turn exists.
    pub fn cancel_zero_trace(self) {
        self.shared.publish(AgentEvent::OperationSettled {
            operation_id: self.shared.operation_id().clone(),
            status: OperationStatus::Cancelled,
            durability: SettlementDurability::Confirmed,
        });
        self.shared.settle(OperationOutcome::Cancelled);
    }

    /// Settles as cancelled after the cancellation transaction committed:
    /// publishes `TurnCancelled` before the terminal event.
    pub fn cancel_committed(self) {
        self.shared.publish(AgentEvent::TurnCancelled {
            turn_id: self.shared.turn_id().clone(),
            reason: self.shared.cancel_reason(),
        });
        self.shared.publish(AgentEvent::OperationSettled {
            operation_id: self.shared.operation_id().clone(),
            status: OperationStatus::Cancelled,
            durability: SettlementDurability::Confirmed,
        });
        self.shared.settle(OperationOutcome::Cancelled);
    }
}
