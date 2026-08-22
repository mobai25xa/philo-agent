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

/// Turn-engine model-call recovery policy. When a model call fails with a
/// [`crate::ModelFailureClass::Recoverable`] error, the engine may re-issue
/// the identical call (same kernel effect) after a bounded full-jitter
/// backoff. Failed attempts commit nothing durable, so recovery never
/// duplicates tool executions or assistant output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryConfig {
    /// Master switch; `false` restores the fail-fast behavior.
    pub enabled: bool,
    /// Additional identical attempts per model call after the first failure
    /// (`0` keeps one attempt per call).
    pub max_retries: u32,
    /// Exponential backoff base delay for the first retry.
    pub backoff_base_ms: u64,
    /// Exponential backoff cap.
    pub backoff_max_ms: u64,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: 3,
            backoff_base_ms: 500,
            backoff_max_ms: 8_000,
        }
    }
}

impl RecoveryConfig {
    /// Full-jitter backoff delay before retry number `retry` (1-based),
    /// capped by `backoff_max_ms`.
    pub fn backoff_delay(&self, retry: u32) -> std::time::Duration {
        let exponent = retry.saturating_sub(1).min(16);
        let cap = self.backoff_max_ms.max(self.backoff_base_ms);
        let bounded = self
            .backoff_base_ms
            .saturating_mul(1_u64 << exponent)
            .min(cap);
        let jitter = bounded / 4;
        let delay = if jitter == 0 {
            bounded
        } else {
            let offset = u64::from(backoff_jitter(jitter));
            bounded - offset
        };
        std::time::Duration::from_millis(delay)
    }
}

/// Dependency-free pseudo-random offset in `[0, bound)` for full-jitter
/// backoff; entropy comes from the wall clock.
fn backoff_jitter(bound: u64) -> u32 {
    thread_local! {
        static STATE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }
    STATE.with(|state| {
        let mut value = state.get();
        if value == 0 {
            value = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.subsec_nanos() as u64 ^ (since.as_secs() << 20))
                .unwrap_or(0x9E37_79B9_7F4A_7C15)
                | 1;
        }
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        state.set(value);
        (value % bound.max(1)) as u32
    })
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
    /// like `cancel()` — effect points stay the M6 injection points — with
    /// reason `Timeout`. In-flight tools are signalled immediately and
    /// waited only up to [`RuntimeConfig::tool_cancel_grace`].
    /// `None` (the default) disables the timeout entirely.
    pub operation_timeout: Option<std::time::Duration>,
    /// Shared grace for in-flight tool invokes after cancel is signalled.
    /// Not a CLI/TOML key. `ZERO` drops still-pending futures on the next
    /// poll; a same-poll `Ready` still wins.
    pub tool_cancel_grace: std::time::Duration,
    /// Pre-turn and manual context-compaction policy (M13).
    pub compaction: CompactionConfig,
    /// Model-call recovery policy applied by the turn engine.
    pub recovery: RecoveryConfig,
}

/// Default shared in-flight cancel grace.
pub const DEFAULT_TOOL_CANCEL_GRACE: std::time::Duration = std::time::Duration::from_millis(300);

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            system_prompt: String::new(),
            model_target: String::new(),
            generation: GenerationConfig::default(),
            max_tool_rounds: DEFAULT_MAX_TOOL_ROUNDS,
            max_parallel_tool_calls: DEFAULT_MAX_PARALLEL_TOOL_CALLS,
            operation_timeout: None,
            tool_cancel_grace: DEFAULT_TOOL_CANCEL_GRACE,
            compaction: CompactionConfig::default(),
            recovery: RecoveryConfig::default(),
        }
    }
}
