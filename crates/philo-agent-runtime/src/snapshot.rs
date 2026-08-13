use crate::{ModelCallId, ModelMessage, OperationId, SessionId, TurnId};
use philo_session::SessionRevision;
use philo_tools::ToolDefinition;

/// Requested reasoning effort tier. Runtime-owned vocabulary; adapters map
/// it onto provider-native controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    VeryHigh,
    Maximum,
}

/// Requested tool-choice mode (M10). Runtime-owned vocabulary aligned with
/// the SDK's; the semantic mapping belongs to the model adapter. On
/// tool-disabled calls (kernel `tools_allowed = false`) tool disabling wins
/// and this configuration has no effect.
///
/// Known usage constraint (caller-owned): `Required` / `Specific` force a
/// tool call on every round, so a turn cannot end with a final text answer
/// and will terminate at the round limit. The default is `Auto`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Specific { name: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct GenerationConfig {
    pub max_output_tokens: u32,
    pub temperature: f32,
    /// Requested reasoning effort; `None` keeps the provider default and the
    /// pre-M7 request shape. Frozen into the TurnSnapshot per turn.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Requested tool-choice mode; `Auto` keeps the pre-M10 request shape.
    /// Frozen into the TurnSnapshot per turn.
    pub tool_choice: ToolChoice,
}
impl Default for GenerationConfig {
    fn default() -> Self {
        Self {
            max_output_tokens: 1024,
            temperature: 0.0,
            reasoning_effort: None,
            tool_choice: ToolChoice::Auto,
        }
    }
}

/// Default upper bound of tool rounds per turn.
pub const DEFAULT_MAX_TOOL_ROUNDS: u32 = 8;

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeConfig {
    pub system_prompt: String,
    pub model_target: String,
    pub generation: GenerationConfig,
    /// Upper bound of tool rounds frozen into every turn; `0` never exposes tools.
    pub max_tool_rounds: u32,
    /// Operation-level automatic cancellation (M11). Timing starts when the
    /// operation is dequeued and actually starts driving (`Queued` waiting
    /// is excluded). On expiry the runtime requests cancellation exactly
    /// like `cancel()` — effect points stay the M6 injection points, an
    /// executing tool call runs to completion — with reason `Timeout`.
    /// `None` (the default) disables the timeout entirely.
    pub operation_timeout: Option<std::time::Duration>,
}

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
