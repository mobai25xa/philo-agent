//! Runtime configuration vocabulary frozen into every turn.

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

/// Default upper bound of concurrent tool invocations in one batch.
pub const DEFAULT_MAX_PARALLEL_TOOL_CALLS: u32 = 8;

/// Context compaction policy. Budget resolution belongs to the assembly
/// root; `None` disables automatic compaction while leaving manual
/// compaction available.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionConfig {
    pub context_budget: Option<u64>,
    pub auto_threshold: f32,
    pub keep_recent_turns: u32,
    pub estimate_bytes_per_token: u32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            context_budget: None,
            auto_threshold: 0.8,
            keep_recent_turns: 4,
            estimate_bytes_per_token: 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeConfig {
    pub system_prompt: String,
    pub model_target: String,
    pub generation: GenerationConfig,
    /// Upper bound of tool rounds frozen into every turn; `0` never exposes tools.
    pub max_tool_rounds: u32,
    /// Upper bound of concurrent `ToolPort::invoke` calls in one batch.
    /// Frozen into every turn; minimum `1` uses the serial path.
    pub max_parallel_tool_calls: u32,
    /// Operation-level automatic cancellation (M11). Timing starts when the
    /// operation is dequeued and actually starts driving (`Queued` waiting
    /// is excluded). On expiry the runtime requests cancellation exactly
    /// like `cancel()` — effect points stay the M6 injection points, an
    /// executing tool call runs to completion — with reason `Timeout`.
    /// `None` (the default) disables the timeout entirely.
    pub operation_timeout: Option<std::time::Duration>,
    /// Pre-turn and manual context-compaction policy (M13).
    pub compaction: CompactionConfig,
}
