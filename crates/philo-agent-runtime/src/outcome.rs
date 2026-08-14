//! Phase, status, failure, and outcome vocabulary of one operation.

use crate::{AssistantMessage, OperationId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelCallPhase {
    Starting,
    WaitingForFirstOutput,
    Streaming,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunningToolBatchPhase {
    Preparing,
    Executing { index: usize },
    CommittingResults,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationPhase {
    /// Waiting in the FIFO follow-up queue; no turn exists yet.
    Queued,
    PreparingTurn,
    RunningModelCall(ModelCallPhase),
    RunningToolBatch(RunningToolBatchPhase),
    Finalizing,
    Settled(OperationStatus),
}

/// Lets a by-value phase compare against `&OperationPhase` expectations.
impl PartialEq<&OperationPhase> for OperationPhase {
    fn eq(&self, other: &&OperationPhase) -> bool {
        self == *other
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationStatus {
    Succeeded,
    Failed,
    Cancelled,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementDurability {
    Confirmed,
    Unconfirmed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentFailureKind {
    ModelCall,
    InvalidModelOutput,
    ToolExecution,
    Persistence,
    RuntimeDriver,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentFailure {
    kind: AgentFailureKind,
    message: String,
}
impl AgentFailure {
    pub fn new(kind: AgentFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    /// Shorthand for an [`AgentFailureKind::InvalidModelOutput`] failure.
    pub(crate) fn invalid_model_output(message: impl Into<String>) -> Self {
        Self::new(AgentFailureKind::InvalidModelOutput, message)
    }
    /// Shorthand for an [`AgentFailureKind::RuntimeDriver`] failure.
    pub(crate) fn runtime_driver(message: impl Into<String>) -> Self {
        Self::new(AgentFailureKind::RuntimeDriver, message)
    }
    pub fn kind(&self) -> AgentFailureKind {
        self.kind
    }
    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationOutcome {
    Succeeded {
        assistant: AssistantMessage,
    },
    Failed {
        failure: AgentFailure,
        durability: SettlementDurability,
    },
    /// The operation ended by user request; a normal terminal outcome.
    Cancelled,
}

/// Read-only observation of whether the runtime is driving an operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentAvailability {
    Idle,
    Busy { operation_id: OperationId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentError {
    message: String,
}
impl AgentError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
    pub fn message(&self) -> &str {
        &self.message
    }
}
