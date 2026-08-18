//! Runtime-wide events wrapping [`crate::AgentEvent`] plus availability,
//! maintenance, fault, lag, and epoch lifecycle.

use crate::{
    AgentAvailability, AgentEvent, CompactionError, CompactionReport, DiagnosticId, MaintenanceId,
    OperationId, OperationStatus, RuntimeEpoch, SessionId, SettlementDurability,
    SettlementRevision, TurnId,
};

/// Terminal result of a maintenance task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MaintenanceResult {
    Compacted(CompactionReport),
    Failed(CompactionError),
    Cancelled,
    Panicked { diagnostic_id: DiagnosticId },
}

/// Why one runtime epoch ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EpochEndReason {
    Shutdown,
    CoordinatorFault,
    EventSinkClosed,
}

/// Events emitted on the single bounded runtime subscription.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeEvent {
    OperationAccepted {
        operation_id: OperationId,
        session_id: SessionId,
        turn_id: TurnId,
    },
    OperationSettled {
        operation_id: OperationId,
        session_id: SessionId,
        status: OperationStatus,
        durability: SettlementDurability,
        session_revision: SettlementRevision,
    },
    Agent(AgentEvent),
    AvailabilityChanged {
        availability: AgentAvailability,
        queued: usize,
    },
    MaintenanceAccepted {
        id: MaintenanceId,
        session_id: SessionId,
    },
    MaintenanceStarted {
        id: MaintenanceId,
    },
    MaintenanceProgress {
        id: MaintenanceId,
        message: String,
    },
    MaintenanceSettled {
        id: MaintenanceId,
        session_id: SessionId,
        result: MaintenanceResult,
    },
    RuntimeFault {
        diagnostic_id: DiagnosticId,
        message: String,
    },
    SubscriptionLagged {
        dropped: usize,
    },
    EpochEnded {
        epoch: RuntimeEpoch,
        reason: EpochEndReason,
        forced_count: usize,
    },
}

/// Outcome of [`crate::RuntimeEventReceiver::try_recv`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TryRecvError {
    Empty,
    Closed,
}

pub(crate) fn is_mergeable(event: &RuntimeEvent) -> bool {
    matches!(
        event,
        RuntimeEvent::Agent(
            AgentEvent::TextDelta { .. }
                | AgentEvent::ReasoningDelta { .. }
                | AgentEvent::ToolExecutionProgress { .. }
                | AgentEvent::ModelUsageUpdated { .. }
        ) | RuntimeEvent::AvailabilityChanged { .. }
            | RuntimeEvent::MaintenanceProgress { .. }
    )
}

pub(crate) fn merge_events(held: RuntimeEvent, next: RuntimeEvent) -> Option<RuntimeEvent> {
    match (held, next) {
        (
            RuntimeEvent::Agent(AgentEvent::TextDelta { delta: left }),
            RuntimeEvent::Agent(AgentEvent::TextDelta { delta: right }),
        ) => {
            let mut delta = left;
            delta.push_str(&right);
            if delta.len() > crate::bounds::DELTA_MERGE_CHUNK_MAX {
                delta.truncate(crate::bounds::DELTA_MERGE_CHUNK_MAX);
            }
            Some(RuntimeEvent::Agent(AgentEvent::TextDelta { delta }))
        }
        (
            RuntimeEvent::Agent(AgentEvent::ReasoningDelta {
                model_call_id: left_id,
                text: left,
            }),
            RuntimeEvent::Agent(AgentEvent::ReasoningDelta {
                model_call_id: right_id,
                text: right,
            }),
        ) if left_id == right_id => {
            let mut text = left;
            text.push_str(&right);
            if text.len() > crate::bounds::DELTA_MERGE_CHUNK_MAX {
                text.truncate(crate::bounds::DELTA_MERGE_CHUNK_MAX);
            }
            Some(RuntimeEvent::Agent(AgentEvent::ReasoningDelta {
                model_call_id: left_id,
                text,
            }))
        }
        (
            RuntimeEvent::Agent(AgentEvent::ToolExecutionProgress {
                tool_call_id: left, ..
            }),
            RuntimeEvent::Agent(right),
        ) if matches!(
            &right,
            AgentEvent::ToolExecutionProgress { tool_call_id, .. } if *tool_call_id == left
        ) =>
        {
            Some(RuntimeEvent::Agent(right))
        }
        (
            RuntimeEvent::Agent(AgentEvent::ModelUsageUpdated {
                model_call_id: left,
                ..
            }),
            RuntimeEvent::Agent(right),
        ) if matches!(
            &right,
            AgentEvent::ModelUsageUpdated { model_call_id, .. } if *model_call_id == left
        ) =>
        {
            Some(RuntimeEvent::Agent(right))
        }
        (
            RuntimeEvent::AvailabilityChanged { .. },
            right @ RuntimeEvent::AvailabilityChanged { .. },
        ) => Some(right),
        (RuntimeEvent::MaintenanceProgress { id: left, .. }, right) => match &right {
            RuntimeEvent::MaintenanceProgress { id, .. } if *id == left => Some(right),
            _ => None,
        },
        _ => None,
    }
}
