//! Frozen per-turn and per-call facts handed to the model port.

use crate::{GenerationConfig, ModelCallId, ModelMessage, OperationId, SessionId, TurnId};
use philo_session::SessionRevision;
use philo_tools::ToolDefinition;

#[derive(Clone, Debug, PartialEq)]
pub struct TurnSnapshot {
    pub session_id: SessionId,
    pub session_revision: SessionRevision,
    pub context_messages: Vec<ModelMessage>,
    pub system_prompt: String,
    pub model_target: String,
    pub generation: GenerationConfig,
    pub tools: Vec<ToolDefinition>,
    pub max_tool_rounds: u32,
    /// Concurrent invoke cap frozen for this turn; `1` is the serial path.
    pub max_parallel_tool_calls: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelCallSnapshot {
    /// Session owning this call. Model adapters use it to scope provider
    /// replay sidecars and, for openai-chat, as the stable cache-routing
    /// identity (`prompt_cache_key` / session-affinity headers).
    pub session_id: SessionId,
    /// Opaque active-path leaf before this call. It prevents replay state
    /// from one context branch being treated as state for another branch.
    pub context_fingerprint: String,
    /// Whether this call produces assistant history that must be persisted.
    /// Synthetic maintenance calls set this to false.
    pub persist_replay: bool,
    pub operation_id: OperationId,
    pub turn_id: TurnId,
    pub model_call_id: ModelCallId,
    /// One-based sequence number of this logical call within the turn.
    pub model_call_index: u32,
    pub session_revision: SessionRevision,
    pub messages: Vec<ModelMessage>,
    /// Frozen tool definitions when the kernel allows tools, empty otherwise.
    pub tools: Vec<ToolDefinition>,
    pub model_target: String,
    pub generation: GenerationConfig,
    /// Frozen concurrent invoke cap; adapters send `Allow` when this is `> 1`.
    pub max_parallel_tool_calls: u32,
}
