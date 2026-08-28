//! Model-visible context projection of the active linear path.

use crate::entry::{
    EntryId, OperationId, SessionAssistantBlock, SessionId, SessionRevision, SessionUserPart,
    ToolBatchId, ToolCallId, TurnId,
};
use crate::generation_choice::SessionGenerationChoice;
use crate::tool_entry::ToolResultOutcome;
use crate::usage::SessionTokenUsage;

/// A model-visible message projected from the active linear path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextMessage {
    /// A durable summary replacing an earlier model-visible prefix.
    Summary { text: String },
    /// A user message carrying the turn's full multi-part payload.
    User { parts: Vec<SessionUserPart> },
    /// A final assistant message. Blocks are text-only; an empty list is
    /// empty final text.
    Assistant { blocks: Vec<SessionAssistantBlock> },
    /// An assistant tool-call batch in source order, including any
    /// interleaved text blocks.
    AssistantToolCalls {
        tool_batch_id: ToolBatchId,
        blocks: Vec<SessionAssistantBlock>,
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
    pub(crate) title: Option<String>,
    pub(crate) messages: Vec<ContextMessage>,
    pub(crate) open_turns: Vec<OpenTurnInfo>,
    pub(crate) settled_turns: Vec<(OperationId, TurnId)>,
    pub(crate) settled_turn_boundaries: Vec<EntryId>,
    pub(crate) latest_compaction_boundary: Option<EntryId>,
    pub(crate) latest_usage: Option<SessionTokenUsage>,
    pub(crate) latest_generation: Option<SessionGenerationChoice>,
}

impl SessionContextView {
    /// Returns the viewed session.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Returns the resolved display title, if any.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
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

    /// Returns `(operation_id, turn_id)` for operations with a durable
    /// terminal outcome. Order is stable by operation id.
    pub fn settled_turns(&self) -> &[(OperationId, TurnId)] {
        &self.settled_turns
    }

    /// Returns every settled operation entry boundary in source order.
    /// Entry IDs remain opaque; runtimes use this sequence to choose a
    /// whole-turn compaction boundary without inspecting identifier syntax.
    pub fn settled_turn_boundaries(&self) -> &[EntryId] {
        &self.settled_turn_boundaries
    }

    /// Returns the boundary of the newest durable compaction, if any.
    pub fn latest_compaction_boundary(&self) -> Option<&EntryId> {
        self.latest_compaction_boundary.as_ref()
    }

    /// Returns the token usage recorded at the newest settled turn, if any.
    /// `None` for sessions with no settled turns or when the provider did
    /// not report usage.
    pub fn latest_usage(&self) -> Option<SessionTokenUsage> {
        self.latest_usage
    }

    /// Returns the generation choice recorded at the newest settled turn,
    /// if any. `None` for sessions with no settled turns or when the choice
    /// was not recorded (legacy logs). Used for cross-process recovery to
    /// rebuild the session's `RuntimeGeneration`.
    pub fn latest_generation(&self) -> Option<&SessionGenerationChoice> {
        self.latest_generation.as_ref()
    }
}
