//! Event publication and one-shot settlement of a driven operation.

use super::shared::OperationShared;
use crate::{
    AgentEvent, AgentFailure, AssistantMessage, OperationId, OperationOutcome, OperationPhase,
    OperationStatus, SettlementDurability, SettlementRevision, TurnId,
};
use philo_session::CancelReason;
use std::sync::Arc;
use std::time::Duration;

/// Publishes events and phase transitions while an operation is driven and
/// settles the shared state exactly once. Publication is immediate: the
/// barrier-ordering guarantee lives at the call sites, which only push
/// after the corresponding barrier committed.
pub(crate) struct OperationPublisher {
    shared: Arc<OperationShared>,
}

impl OperationPublisher {
    /// Starts driving: publishes `OperationStarted` and arms the automatic
    /// cancellation deadline. `TurnStarted` follows separately through
    /// [`OperationPublisher::turn_started`] so seal notifications (M11) can
    /// land between the two.
    pub async fn begin(shared: Arc<OperationShared>, operation_timeout: Option<Duration>) -> Self {
        shared.arm_deadline(operation_timeout);
        shared
            .publish(AgentEvent::OperationStarted {
                operation_id: shared.operation_id().clone(),
            })
            .await;
        shared.set_phase(OperationPhase::PreparingTurn);
        Self { shared }
    }

    /// Publishes `TurnStarted` once every stale prior turn is sealed.
    pub async fn turn_started(&self) {
        self.shared
            .publish(AgentEvent::TurnStarted {
                turn_id: self.shared.turn_id().clone(),
            })
            .await;
    }

    /// Publishes `PriorTurnSealed` after one seal transaction committed.
    pub async fn prior_turn_sealed(&self, turn_id: TurnId) {
        self.shared
            .publish(AgentEvent::PriorTurnSealed { turn_id })
            .await;
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

    pub async fn push(&self, event: AgentEvent) {
        self.shared.publish(event).await;
    }

    pub fn set_phase(&self, phase: OperationPhase) {
        self.shared.set_phase(phase);
    }

    /// Publishes `CancellationRequested` when a cancel signal is observed at
    /// an injection point.
    pub async fn cancellation_observed(&self) {
        self.shared
            .publish(AgentEvent::CancellationRequested {
                operation_id: self.shared.operation_id().clone(),
                reason: self.shared.cancel_reason(),
            })
            .await;
    }

    pub async fn succeed(
        self,
        assistant: AssistantMessage,
        session_revision: philo_session::SessionRevision,
    ) {
        if !self.shared.settle(OperationOutcome::Succeeded {
            assistant: assistant.clone(),
        }) {
            return;
        }
        self.shared
            .publish(AgentEvent::AssistantMessageCompleted {
                turn_id: self.shared.turn_id().clone(),
                message: assistant,
            })
            .await;
        self.publish_settled(
            OperationStatus::Succeeded,
            SettlementDurability::Confirmed,
            SettlementRevision::Committed(session_revision),
        )
        .await;
    }

    pub async fn fail_confirmed(
        self,
        failure: AgentFailure,
        session_revision: philo_session::SessionRevision,
    ) {
        if !self.shared.settle(OperationOutcome::Failed {
            failure: failure.clone(),
            durability: SettlementDurability::Confirmed,
        }) {
            return;
        }
        self.shared
            .publish(AgentEvent::TurnFailed {
                turn_id: self.shared.turn_id().clone(),
                failure,
            })
            .await;
        self.publish_settled(
            OperationStatus::Failed,
            SettlementDurability::Confirmed,
            SettlementRevision::Committed(session_revision),
        )
        .await;
    }

    pub async fn fail_unconfirmed(self, failure: AgentFailure) {
        self.fail(
            failure,
            SettlementDurability::Unconfirmed,
            SettlementRevision::Unchanged,
        )
        .await;
    }

    async fn fail(
        self,
        failure: AgentFailure,
        durability: SettlementDurability,
        session_revision: SettlementRevision,
    ) {
        if !self.shared.settle(OperationOutcome::Failed {
            failure,
            durability,
        }) {
            return;
        }
        self.publish_settled(OperationStatus::Failed, durability, session_revision)
            .await;
    }

    /// Settles as cancelled before any turn fact was persisted: no
    /// `TurnCancelled` because durably no turn exists.
    pub async fn cancel_zero_trace(self) {
        if !self.shared.settle(OperationOutcome::Cancelled) {
            return;
        }
        self.publish_settled(
            OperationStatus::Cancelled,
            SettlementDurability::Confirmed,
            SettlementRevision::Unchanged,
        )
        .await;
    }

    /// Settles as cancelled after the cancellation transaction committed:
    /// publishes `TurnCancelled` before the terminal event.
    pub async fn cancel_committed(self, session_revision: philo_session::SessionRevision) {
        if !self.shared.settle(OperationOutcome::Cancelled) {
            return;
        }
        self.shared
            .publish(AgentEvent::TurnCancelled {
                turn_id: self.shared.turn_id().clone(),
                reason: self.shared.cancel_reason(),
            })
            .await;
        self.publish_settled(
            OperationStatus::Cancelled,
            SettlementDurability::Confirmed,
            SettlementRevision::Committed(session_revision),
        )
        .await;
    }

    /// Publishes the terminal settlement. `Committed` only after a Session
    /// transaction actually advanced; never invent a revision.
    async fn publish_settled(
        &self,
        status: OperationStatus,
        durability: SettlementDurability,
        session_revision: SettlementRevision,
    ) {
        self.shared
            .publish(AgentEvent::OperationSettled {
                operation_id: self.shared.operation_id().clone(),
                status,
                durability,
                session_revision,
            })
            .await;
    }
}
