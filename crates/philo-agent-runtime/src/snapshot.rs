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
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelCallSnapshot {
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
}
