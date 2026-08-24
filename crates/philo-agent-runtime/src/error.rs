//! Public error and admission vocabulary for the self-driven runtime.

use crate::{AgentAvailability, DiagnosticId, OperationId, OperationStatus, SettlementDurability};

/// Why [`crate::AgentRuntime::start`] failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartError {
    InvalidBounds,
    RuntimeUnavailable { message: String },
}

impl StartError {
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidBounds => "channel bounds must be greater than zero",
            Self::RuntimeUnavailable { message } => message,
        }
    }
}

/// Why an operation was not admitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionError {
    QueueFull,
    Backpressured,
    ShuttingDown,
    RuntimeStopped,
}

impl AdmissionError {
    pub fn message(&self) -> &str {
        match self {
            Self::QueueFull => "runtime operation queue is full",
            Self::Backpressured => "runtime command channel is full",
            Self::ShuttingDown => "runtime is shutting down",
            Self::RuntimeStopped => "runtime epoch has ended",
        }
    }
}

/// Result of an explicit cancel command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CancelResult {
    Requested,
    QueuedCancelled,
    TooLate,
    AlreadySettled,
    UnknownOperation,
    Backpressured,
    RuntimeStopped,
}

/// Why maintenance was not accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MaintenanceError {
    Unavailable { availability: AgentAvailability },
    Backpressured,
    ShuttingDown,
    RuntimeStopped,
}

impl MaintenanceError {
    pub fn message(&self) -> &str {
        match self {
            Self::Unavailable { .. } => "runtime is not available for compaction",
            Self::Backpressured => "runtime command channel is full",
            Self::ShuttingDown => "runtime is shutting down",
            Self::RuntimeStopped => "runtime epoch has ended",
        }
    }
}

/// Driver-side invariant failure. Replaces production `expect("mutex")`
/// / `expect("effect")` panics; panic supervision remains the last resort.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverInvariantError {
    message: String,
}

impl DriverInvariantError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub(crate) fn into_failure(self) -> crate::AgentFailure {
        use crate::{FailureDomain, FailureStage, RetryDisposition};
        crate::AgentFailure::new(
            "engine.invariant_violation",
            FailureDomain::Internal,
            FailureStage::TurnEngine,
            RetryDisposition::Never,
            "an internal driver invariant was violated",
            self.message,
        )
    }
}

/// How a driven operation ended. Exhaustive: the coordinator must settle
/// after any of these, including panic and abort.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriverExit {
    Succeeded,
    FailedConfirmed,
    FailedUnconfirmed,
    CancelledConfirmed,
    Panicked { diagnostic_id: DiagnosticId },
    Aborted { diagnostic_id: DiagnosticId },
}

/// Forced terminal applied to one accepted operation when an epoch ends
/// without a normal settlement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForcedSettlement {
    pub operation_id: OperationId,
    pub session_id: crate::SessionId,
    pub status: OperationStatus,
    pub durability: SettlementDurability,
    pub diagnostic_id: DiagnosticId,
}

/// Why [`crate::RuntimeHandle::shutdown`] did not return a supervisor report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShutdownError {
    RuntimeGone,
    SupervisorPanicked,
    DeadlineExceeded { pending: Vec<String> },
}

/// Diagnostic captured while an epoch is finalized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShutdownDiagnostic {
    pub message: String,
}
