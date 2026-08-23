//! Serde-only schema v2 record definitions.
//!
//! Field names and shapes are part of the persistent compatibility promise
//! and are pinned by golden tests. v2 accepts only `parts` / `blocks` and
//! requires a `reason` on cancelled outcomes.

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
    /// v2 writes and accepts only `parts`. A leftover `content` field is
    /// unknown and fails decode.
    UserMessage(UserMessageRecord),
    AssistantToolCallBatch(AssistantToolCallBatchRecord),
    ToolResult {
        turn_id: String,
        tool_batch_id: String,
        result: ToolResultRecord,
    },
    AssistantMessage(AssistantMessageRecord),
    TurnFailure {
        turn_id: String,
        failure: FailureRecord,
    },
    /// Cancelled outcomes must carry a reason; absence is corrupt.
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
    /// Additive v2 record (session titles). Readers older than this variant
    /// reject such logs as corrupt; no envelope version bump was needed
    /// because the shape of existing records is untouched.
    TitleSet { title: String },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserMessageRecord {
    pub turn_id: String,
    pub parts: Vec<UserPartRecord>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssistantToolCallBatchRecord {
    pub turn_id: String,
    pub model_call_id: String,
    pub tool_batch_id: String,
    pub blocks: Vec<AssistantBlockRecord>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssistantMessageRecord {
    pub turn_id: String,
    pub blocks: Vec<AssistantBlockRecord>,
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
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AssistantBlockRecord {
    Text {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
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
