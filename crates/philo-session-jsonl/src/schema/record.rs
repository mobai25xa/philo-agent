//! Serde-only schema v1 record definitions.
//!
//! Field names and shapes are part of the persistent compatibility promise
//! and are pinned by golden tests.

use serde::{Deserialize, Serialize};

/// One committed transaction: exactly one log line.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct TransactionRecord {
    pub v: u64,
    pub revision: u64,
    pub entries: Vec<EntryRecord>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct EntryRecord {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub kind: KindRecord,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum KindRecord {
    OperationStarted {
        operation_id: String,
    },
    TurnStarted {
        operation_id: String,
        turn_id: String,
    },
    /// New files always write `parts`; `content` is the pre-M8 legacy shape.
    UserMessage {
        turn_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parts: Option<Vec<UserPartRecord>>,
    },
    AssistantToolCallBatch {
        turn_id: String,
        model_call_id: String,
        tool_batch_id: String,
        calls: Vec<ToolCallRecord>,
    },
    ToolResult {
        turn_id: String,
        tool_batch_id: String,
        result: ToolResultRecord,
    },
    AssistantMessage {
        turn_id: String,
        content: String,
    },
    TurnFailure {
        turn_id: String,
        failure: FailureRecord,
    },
    /// Cancelled outcomes written since M11 carry a reason; absent means the
    /// legacy user-cancellation source.
    TurnTerminated {
        turn_id: String,
        outcome: OutcomeRecord,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<ReasonRecord>,
    },
    OperationSettled {
        operation_id: String,
        outcome: OutcomeRecord,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<ReasonRecord>,
    },
    Compaction {
        summary: String,
        covers_up_to: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum UserPartRecord {
    Text {
        text: String,
    },
    Image {
        media_type: String,
        artifact: String,
        len: u64,
    },
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ToolCallRecord {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ToolResultRecord {
    pub call_id: String,
    #[serde(flatten)]
    pub outcome: ToolOutcomeRecord,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum ToolOutcomeRecord {
    Success { content: String },
    Error { code: String, message: String },
    Cancelled,
    Interrupted,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct FailureRecord {
    pub kind: FailureKindRecord,
    pub message: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureKindRecord {
    ModelCall,
    InvalidModelOutput,
    Persistence,
    RuntimeDriver,
    ToolExecution,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutcomeRecord {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReasonRecord {
    User,
    Timeout,
    Abandoned,
}
