//! Failure vocabulary mapping across the runtime, kernel, and session.

use crate::{AgentFailure, DurableFailureKind, FailureDomain, FailureStage, RetryDisposition};
use philo_agent_kernel as kernel;
use philo_session as session;

pub(crate) fn kernel_failure(failure: &AgentFailure) -> kernel::TurnFailure {
    let message = failure.diagnostic().to_owned();
    match failure.durable_kind() {
        DurableFailureKind::ModelCall => kernel::TurnFailure::ModelCallFailed { message },
        DurableFailureKind::InvalidModelOutput => {
            kernel::TurnFailure::InvalidModelOutput { message }
        }
        DurableFailureKind::ToolExecution => kernel::TurnFailure::ToolExecutionFailed { message },
        DurableFailureKind::Persistence => kernel::TurnFailure::PersistenceFailed { message },
        DurableFailureKind::RuntimeDriver => kernel::TurnFailure::RuntimeDriverFailed { message },
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

/// Raw store fault observed outside a classified commit point (context
/// reads, list, seal re-read). The code table's `session.*` rows pin the
/// domain and retry advice per [`session::SessionError`] variant.
pub(crate) fn session_failure(context: &str, error: &session::SessionError) -> AgentFailure {
    let (code, domain, retry, summary) = match error {
        session::SessionError::StoreBusy { .. } => (
            "session.store_busy",
            FailureDomain::Storage,
            RetryDisposition::Safe { retry_after_ms: None },
            "the local session store queue is full",
        ),
        session::SessionError::StoreUnavailable { .. } => (
            "session.store_unavailable",
            FailureDomain::Storage,
            RetryDisposition::Never,
            "the local session store is unavailable",
        ),
        session::SessionError::RevisionConflict { .. } => (
            "session.revision_conflict",
            FailureDomain::Internal,
            RetryDisposition::Never,
            "the session changed concurrently",
        ),
        session::SessionError::Validation(_) => (
            "session.validation_rejected",
            FailureDomain::Internal,
            RetryDisposition::Never,
            "the session rejected the transaction",
        ),
    };
    AgentFailure::new(
        code,
        domain,
        FailureStage::SessionStore,
        retry,
        summary,
        format!("{context}: {}", describe_session_error(error)),
    )
}

/// Classified commit-point failure (barrier A/B/C, settlement, cancel,
/// seal). The engine owns the effect-class advice per frozen code-table
/// row; the raw store fault stays in the diagnostic.
pub(crate) fn commit_failure(
    code: &'static str,
    retry: RetryDisposition,
    context: &str,
    error: &session::SessionError,
) -> AgentFailure {
    let summary = match code {
        "engine.barrier_a_commit_failed" => "the turn start could not be recorded",
        "engine.barrier_b_commit_failed" => "the tool call batch could not be recorded",
        "engine.barrier_c_commit_failed" => "the tool results could not be recorded",
        "engine.settlement_commit_failed" => "the successful settlement could not be recorded",
        "engine.cancel_commit_failed" => "the cancellation could not be recorded",
        "engine.seal_commit_failed" => "a stale unfinished turn could not be sealed",
        _ => "a session commit failed",
    };
    AgentFailure::new(
        code,
        FailureDomain::Storage,
        FailureStage::TurnEngine,
        retry,
        summary,
        format!("{context}: {}", describe_session_error(error)),
    )
}

pub(crate) fn describe_session_error(error: &session::SessionError) -> String {
    format!("{error:?}")
}
