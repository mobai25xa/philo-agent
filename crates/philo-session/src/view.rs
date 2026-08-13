//! Model-visible context projection of the active linear path.

use crate::entry::{
    EntryId, OperationId, SessionId, SessionRevision, SessionUserPart, ToolBatchId, ToolCallId,
    TurnId,
};
use crate::tool_entry::{SessionToolCall, ToolResultOutcome};

/// A model-visible message projected from the active linear path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextMessage {
    /// A user message carrying the turn's full multi-part payload.
    User { parts: Vec<SessionUserPart> },
    /// A final assistant message.
    Assistant { content: String },
    /// Assistant tool calls in source order.
    AssistantToolCalls {
        tool_batch_id: ToolBatchId,
        calls: Vec<SessionToolCall>,
    },
    /// A tool result visible to a model.
    ToolResult {
        tool_call_id: ToolCallId,
        outcome: ToolResultOutcome,
    },
}

/// The newest durable tool batch of an open turn that still misses results.
///
/// C_k atomicity guarantees results land all-or-nothing, so the missing
/// calls are always a source-order suffix of the batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnfilledBatch {
    pub(crate) tool_batch_id: ToolBatchId,
    pub(crate) unfilled_call_ids: Vec<ToolCallId>,
}

impl UnfilledBatch {
    /// Returns the durable batch identifier.
    pub fn tool_batch_id(&self) -> &ToolBatchId {
        &self.tool_batch_id
    }

    /// Returns the calls without durable results, in source order.
    pub fn unfilled_call_ids(&self) -> &[ToolCallId] {
        &self.unfilled_call_ids
    }
}

/// One turn without a durable terminal outcome, as seen by the runtime's
/// seal protocol. A session that always terminated cleanly has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenTurnInfo {
    pub(crate) operation_id: OperationId,
    pub(crate) turn_id: TurnId,
    pub(crate) unfilled_batch: Option<UnfilledBatch>,
}

impl OpenTurnInfo {
    /// Returns the owning operation.
    pub fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }

    /// Returns the open turn.
    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    /// Returns the newest batch still missing results, if any.
    pub fn unfilled_batch(&self) -> Option<&UnfilledBatch> {
        self.unfilled_batch.as_ref()
    }
}

/// Stable session context and revision read for a new turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionContextView {
    pub(crate) session_id: SessionId,
    pub(crate) revision: SessionRevision,
    pub(crate) current_leaf: Option<EntryId>,
    pub(crate) messages: Vec<ContextMessage>,
    pub(crate) open_turns: Vec<OpenTurnInfo>,
}

impl SessionContextView {
    /// Returns the viewed session.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Returns the revision captured with this view.
    pub fn revision(&self) -> SessionRevision {
        self.revision
    }

    /// Returns the current leaf captured with this view.
    pub fn current_leaf(&self) -> Option<&EntryId> {
        self.current_leaf.as_ref()
    }

    /// Returns model messages in source order.
    pub fn messages(&self) -> &[ContextMessage] {
        &self.messages
    }

    /// Returns turns without a durable terminal outcome, in source order.
    /// Empty for sessions whose every turn terminated cleanly.
    pub fn open_turns(&self) -> &[OpenTurnInfo] {
        &self.open_turns
    }
}
