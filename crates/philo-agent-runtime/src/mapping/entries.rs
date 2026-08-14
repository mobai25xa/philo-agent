//! Kernel observations projected into durable session entries.

use super::failure::session_failure_from_kernel;
use super::parts::session_user_parts;
use crate::{AgentFailure, OperationId, TurnId};
use philo_agent_kernel as kernel;
use philo_session as session;

pub(crate) fn start_entries(
    observations: &[kernel::KernelObservation],
    operation_id: &OperationId,
    turn_id: &TurnId,
) -> Result<Vec<session::SessionEntryKind>, AgentFailure> {
    let Some((observed, user)) = observations
        .iter()
        .find_map(|observation| match observation {
            kernel::KernelObservation::TurnBegan {
                turn_id,
                user_message,
            } => Some((turn_id, user_message)),
            _ => None,
        })
    else {
        return Err(AgentFailure::runtime_driver(
            "missing TurnBegan observation",
        ));
    };
    if observed.as_str() != turn_id.as_str() {
        return Err(AgentFailure::runtime_driver("unexpected TurnBegan turn id"));
    }
    Ok(vec![
        session::SessionEntryKind::OperationStarted {
            operation_id: session::OperationId::new(operation_id.as_str()),
        },
        session::SessionEntryKind::TurnStarted {
            operation_id: session::OperationId::new(operation_id.as_str()),
            turn_id: session::TurnId::new(turn_id.as_str()),
        },
        session::SessionEntryKind::UserMessage {
            turn_id: session::TurnId::new(turn_id.as_str()),
            parts: session_user_parts(user.parts()),
        },
    ])
}

pub(crate) fn success_entries(
    observations: &[kernel::KernelObservation],
    operation_id: &OperationId,
    turn_id: &TurnId,
) -> Result<Vec<session::SessionEntryKind>, AgentFailure> {
    let Some(output) = observations
        .iter()
        .find_map(|observation| match observation {
            kernel::KernelObservation::AssistantOutputAccepted { output, .. } => Some(output),
            _ => None,
        })
    else {
        return Err(AgentFailure::runtime_driver(
            "missing accepted assistant output",
        ));
    };
    if !observations.iter().any(|observation| {
        matches!(
            observation,
            kernel::KernelObservation::TurnTerminated {
                outcome: kernel::TurnOutcome::Succeeded
            }
        )
    }) {
        return Err(AgentFailure::runtime_driver(
            "missing successful termination",
        ));
    }
    Ok(vec![
        session::SessionEntryKind::AssistantMessage {
            turn_id: session::TurnId::new(turn_id.as_str()),
            content: output.text().to_owned(),
        },
        session::SessionEntryKind::TurnTerminated {
            turn_id: session::TurnId::new(turn_id.as_str()),
            outcome: session::TurnOutcome::Succeeded,
        },
        session::SessionEntryKind::OperationSettled {
            operation_id: session::OperationId::new(operation_id.as_str()),
            outcome: session::OperationOutcome::Succeeded,
        },
    ])
}

pub(crate) fn failure_entries(
    observations: &[kernel::KernelObservation],
    operation_id: &OperationId,
    turn_id: &TurnId,
) -> Result<Vec<session::SessionEntryKind>, AgentFailure> {
    let Some(failure) = observations
        .iter()
        .find_map(|observation| match observation {
            kernel::KernelObservation::TurnFailureAccepted { failure, .. } => Some(failure),
            _ => None,
        })
    else {
        return Err(AgentFailure::runtime_driver("missing accepted failure"));
    };
    Ok(vec![
        session::SessionEntryKind::TurnFailure {
            turn_id: session::TurnId::new(turn_id.as_str()),
            failure: session_failure_from_kernel(failure),
        },
        session::SessionEntryKind::TurnTerminated {
            turn_id: session::TurnId::new(turn_id.as_str()),
            outcome: session::TurnOutcome::Failed,
        },
        session::SessionEntryKind::OperationSettled {
            operation_id: session::OperationId::new(operation_id.as_str()),
            outcome: session::OperationOutcome::Failed,
        },
    ])
}

/// The durable record of one accepted tool-call batch.
pub(crate) fn batch_entry(
    turn_id: &TurnId,
    model_call_id: &kernel::ModelCallId,
    batch_id: &kernel::ToolBatchId,
    calls: &[kernel::KernelToolCall],
) -> session::SessionEntryKind {
    session::SessionEntryKind::AssistantToolCallBatch {
        turn_id: session::TurnId::new(turn_id.as_str()),
        model_call_id: model_call_id.as_str().to_owned(),
        tool_batch_id: session::ToolBatchId::new(batch_id.as_str()),
        calls: calls
            .iter()
            .map(|call| {
                session::SessionToolCall::new(
                    session::ToolCallId::new(call.id().as_str()),
                    call.name(),
                    call.arguments(),
                )
            })
            .collect(),
    }
}
