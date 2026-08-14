//! Coding-scenario assembly for the Philo agent.
//!
//! This crate is the single owner of coding scenario knowledge: which tools
//! are wired up, the coding system prompt, and the runtime configuration
//! defaults. Composition roots (such as `philo-cli`) consume the assembled
//! products and may override individual values; they must not re-state tool
//! lineups or prompt text themselves.
//!
//! The profile deliberately does not depend on `philo-model`: model selection
//! (CallTarget, credentials) is deployment configuration owned by the
//! composition root.
//!
//! # Tool lineup and effect classes (M10)
//!
//! | tool | effect class | suggested default policy |
//! |---|---|---|
//! | `read`  | ReadOnly  | allow (no approval) |
//! | `list`  | ReadOnly  | allow (no approval) |
//! | `grep`  | ReadOnly  | allow (no approval) |
//! | `write` | Workspace | ask |
//! | `edit`  | Workspace | ask |
//! | `shell` | System    | ask (strict) |
//!
//! The suggested policy column is **documentation, not behavior**: approval
//! is an external capability. A composition root that wants gating wraps the
//! registry in its own `ToolPort` decorator and decides per
//! [`philo_tools::EffectClass`]; a denial simply returns
//! `ToolResult::Error` (the loop continues, nothing else changes):
//!
//! ```ignore
//! struct Approval<P: ToolPort> { inner: P }
//! impl<P: ToolPort> ToolPort for Approval<P> {
//!     fn definitions(&self) -> Vec<ToolDefinition> { self.inner.definitions() }
//!     fn invoke<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a> {
//!         let class = self.inner.definitions().iter()
//!             .find(|d| d.name() == invocation.name())
//!             .map(|d| d.effect_class());
//!         Box::pin(async move {
//!             if let Some(EffectClass::System) = class {
//!                 if !user_approves(&invocation) {
//!                     return Ok(RichToolResult::error(
//!                         "denied", "the user declined this command"));
//!                 }
//!             }
//!             self.inner.invoke(invocation).await
//!         })
//!     }
//! }
//! ```

use std::path::PathBuf;

use philo_agent_runtime::{GenerationConfig, RuntimeConfig, ToolChoice};
use philo_tools::ToolRegistry;
use philo_tools_std::{EditTool, GrepTool, ListTool, ReadTool, ShellTool, WriteTool};

/// The coding system prompt shipped with this profile. Initial wording;
/// tuned during real use, not pinned by any contract.
const CODING_SYSTEM_PROMPT: &str = "\
You are a coding assistant working inside the user's workspace. You have \
tools for the full query-modify-verify loop: `read`, `list`, and `grep` to \
inspect the workspace, `write` and `edit` to change files, and `shell` to \
run commands (fixed to the workspace root, no stdin, output truncated). \
Inspect before you change: read the relevant files first, make precise \
edits, and verify with the shell (for example by building or running \
tests) when it matters. Prefer `edit` with a unique context snippet over \
rewriting whole files. A non-zero shell exit code is a normal result: read \
the output and act on it. Be direct and technically precise. If the \
workspace does not contain the information required, say so plainly \
instead of guessing.";

/// Default upper bound of tool rounds for the coding scenario.
pub const DEFAULT_MAX_TOOL_ROUNDS: u32 = 8;

/// Default output-token budget for the coding scenario. Coding answers are
/// longer than chat: the runtime-wide default (1024) is too tight.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4096;

/// Default shell timeout for the coding scenario, in seconds.
pub const DEFAULT_SHELL_TIMEOUT_SECS: u64 = 60;

/// Coding-scenario assembly: the six-tool registry, system prompt, and
/// runtime configuration defaults.
#[derive(Clone, Debug)]
pub struct CodingProfile {
    workspace_root: PathBuf,
    shell_timeout_secs: u64,
}

impl CodingProfile {
    /// Creates a profile whose tools operate under `workspace_root`.
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            shell_timeout_secs: DEFAULT_SHELL_TIMEOUT_SECS,
        }
    }

    /// Overrides the default shell timeout (assembly-level configuration).
    pub fn with_shell_timeout_secs(mut self, seconds: u64) -> Self {
        self.shell_timeout_secs = seconds.max(1);
        self
    }

    /// Returns the workspace root the tools are constrained to.
    pub fn workspace_root(&self) -> &std::path::Path {
        &self.workspace_root
    }

    /// The frozen tool registry for this scenario: the six coding tools,
    /// rooted at the workspace root. Truncation limits use the tools' own
    /// defaults; the shell timeout follows the profile configuration.
    pub fn tool_registry(&self) -> ToolRegistry {
        let root = &self.workspace_root;
        ToolRegistry::builder()
            .register(ReadTool::definition(), ReadTool::new(root))
            .expect("read registers")
            .register(ListTool::definition(), ListTool::new(root))
            .expect("list registers")
            .register(GrepTool::definition(), GrepTool::new(root))
            .expect("grep registers")
            .register(WriteTool::definition(), WriteTool::new(root))
            .expect("write registers")
            .register(EditTool::definition(), EditTool::new(root))
            .expect("edit registers")
            .register(
                ShellTool::definition(),
                ShellTool::new(root).with_default_timeout_secs(self.shell_timeout_secs),
            )
            .expect("shell registers")
            .build()
    }

    /// The coding system prompt text.
    pub fn system_prompt() -> &'static str {
        CODING_SYSTEM_PROMPT
    }

    /// Generation defaults for the coding scenario. `reasoning_effort` stays
    /// `None` (provider default) and `tool_choice` stays `Auto`; callers
    /// override per deployment.
    pub fn generation_defaults() -> GenerationConfig {
        GenerationConfig {
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            temperature: 0.0,
            reasoning_effort: None,
            tool_choice: ToolChoice::Auto,
        }
    }

    /// Assembles a complete `RuntimeConfig` for a deployment-chosen model
    /// target, applying this profile's defaults. Callers override single
    /// fields afterwards (flag > env > profile default).
    pub fn runtime_config(&self, model_target: impl Into<String>) -> RuntimeConfig {
        RuntimeConfig {
            system_prompt: Self::system_prompt().to_owned(),
            model_target: model_target.into(),
            generation: Self::generation_defaults(),
            max_tool_rounds: DEFAULT_MAX_TOOL_ROUNDS,
            // The operation timeout is deployment configuration, not
            // scenario knowledge: the profile keeps it disabled.
            operation_timeout: None,
            compaction: Default::default(),
        }
    }
}
