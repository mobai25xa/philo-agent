//! Failure vocabulary mapping across the runtime, kernel, and session.

use crate::{AgentFailure, AgentFailureKind};
use philo_agent_kernel as kernel;
use philo_session as session;

pub(crate) fn kernel_failure(failure: &AgentFailure) -> kernel::TurnFailure {
    match failure.kind() {
        AgentFailureKind::ModelCall => kernel::TurnFailure::ModelCallFailed {
            message: failure.message().to_owned(),
        },
        AgentFailureKind::InvalidModelOutput => kernel::TurnFailure::InvalidModelOutput {
            message: failure.message().to_owned(),
        },
        AgentFailureKind::ToolExecution => kernel::TurnFailure::ToolExecutionFailed {
            message: failure.message().to_owned(),
        },
        AgentFailureKind::Persistence => kernel::TurnFailure::PersistenceFailed {
            message: failure.message().to_owned(),
        },
        AgentFailureKind::RuntimeDriver => kernel::TurnFailure::RuntimeDriverFailed {
            message: failure.message().to_owned(),
        },
    }
}

pub(super) fn session_failure_from_kernel(failure: &kernel::TurnFailure) -> session::TurnFailure {
    match failure {
        kernel::TurnFailure::ModelCallFailed { message } => {
            session::TurnFailure::new(session::TurnFailureKind::ModelCall, message)
        }
        kernel::TurnFailure::InvalidModelOutput { message } => {
            session::TurnFailure::new(session::TurnFailureKind::InvalidModelOutput, message)
        }
        kernel::TurnFailure::ToolExecutionFailed { message } => {
            session::TurnFailure::new(session::TurnFailureKind::ToolExecution, message)
        }
        kernel::TurnFailure::PersistenceFailed { message } => {
            session::TurnFailure::new(session::TurnFailureKind::Persistence, message)
        }
        kernel::TurnFailure::RuntimeDriverFailed { message } => {
            session::TurnFailure::new(session::TurnFailureKind::RuntimeDriver, message)
        }
    }
}

pub(crate) fn session_failure(context: &str, error: &session::SessionError) -> AgentFailure {
    AgentFailure::new(
        AgentFailureKind::Persistence,
        format!("{context}: {}", describe_session_error(error)),
    )
}

pub(crate) fn describe_session_error(error: &session::SessionError) -> String {
    format!("{error:?}")
}
