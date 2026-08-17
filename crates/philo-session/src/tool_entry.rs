//! Durable tool-call and tool-result payloads embedded in session entries.

use crate::entry::ToolCallId;

/// One model-requested tool call persisted before execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionToolCall {
    id: ToolCallId,
    name: String,
    arguments: String,
}

impl SessionToolCall {
    /// Creates a durable tool call with the model's raw arguments text.
    pub fn new(id: ToolCallId, name: impl Into<String>, arguments: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    /// Returns the durable tool call identifier.
    pub fn id(&self) -> &ToolCallId {
        &self.id
    }

    /// Returns the requested tool name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the raw arguments text exactly as produced by the model.
    pub fn arguments(&self) -> &str {
        &self.arguments
    }
}

/// Durable outcome of one tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolResultOutcome {
    /// The tool produced a text result.
    Success { content: String },
    /// The tool failed recoverably with a stable code and message.
    Error { code: String, message: String },
    /// The call never executed because its turn was cancelled. The fact is
    /// fully expressed by the variant; model-facing rendering is a
    /// Runtime/Adapter mapping concern.
    Cancelled,
    /// The process was interrupted while the call's batch was outstanding,
    /// or a live Runtime stopped an already-started call without a
    /// trustworthy complete result: whether the call executed is unknown
    /// (its side effects may have happened without the result reaching
    /// durable storage). Distinct from [`ToolResultOutcome::Cancelled`],
    /// which asserts the call never ran. Model-facing rendering is a
    /// Runtime/Adapter mapping concern.
    Interrupted,
}

/// One durable tool result matched to its originating call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionToolResult {
    call_id: ToolCallId,
    outcome: ToolResultOutcome,
}

impl SessionToolResult {
    /// Creates a successful text result.
    pub fn success(call_id: ToolCallId, content: impl Into<String>) -> Self {
        Self {
            call_id,
            outcome: ToolResultOutcome::Success {
                content: content.into(),
            },
        }
    }

    /// Creates a recoverable error result.
    pub fn error(call_id: ToolCallId, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            call_id,
            outcome: ToolResultOutcome::Error {
                code: code.into(),
                message: message.into(),
            },
        }
    }

    /// Creates a cancelled marker for a call that never executed.
    pub fn cancelled(call_id: ToolCallId) -> Self {
        Self {
            call_id,
            outcome: ToolResultOutcome::Cancelled,
        }
    }

    /// Creates an interrupted marker for a call whose execution state is
    /// unknown after a process interruption.
    pub fn interrupted(call_id: ToolCallId) -> Self {
        Self {
            call_id,
            outcome: ToolResultOutcome::Interrupted,
        }
    }

    /// Returns the originating tool call identifier.
    pub fn call_id(&self) -> &ToolCallId {
        &self.call_id
    }

    /// Returns the durable outcome.
    pub fn outcome(&self) -> &ToolResultOutcome {
        &self.outcome
    }
}
