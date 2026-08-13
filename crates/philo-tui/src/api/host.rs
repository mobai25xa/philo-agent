//! The host interface: everything the TUI needs from the composition root.
//!
//! `philo-tui` is a pure presentation layer. It never constructs models,
//! stores, or profiles; the composition root implements [`TuiHost`] and the
//! TUI consumes runtime handles and read-only views through it. Snapshot and
//! interaction tests drive the TUI with a fake host.

use philo_agent_runtime::{
    AgentError, OperationHandle, ReasoningEffort, RuntimeFuture, SessionId, ToolDefinition,
    UserMessage,
};
use philo_session::SessionContextView;

use super::confirmation::ConfirmationChannel;

/// A host-side failure surfaced to the user as an error line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostError {
    message: String,
}

impl HostError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

/// One effective configuration entry with its source layer, for `/config`.
/// Values never contain secrets: the config system only stores environment
/// variable names, and the host must uphold that here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigEntry {
    pub key: String,
    pub value: String,
    /// The layer the effective value came from (flag / env / project /
    /// global / default).
    pub source: String,
}

/// Object-safe injection surface implemented by the composition root.
///
/// The TUI holds no ModelPort, SessionStore, or profile types: prompting
/// goes through the runtime handle this interface returns, and every read
/// is a public, read-only view.
pub trait TuiHost: Send + Sync {
    /// Forwards one user prompt to the runtime (M6 queueing applies).
    fn prompt<'a>(
        &'a self,
        session_id: SessionId,
        message: UserMessage,
    ) -> RuntimeFuture<'a, Result<OperationHandle, AgentError>>;

    /// Enumerates known session ids (read-only).
    fn list_sessions(&self) -> Result<Vec<philo_session::SessionId>, HostError>;

    /// Reads a session's context view for history rendering and previews.
    fn context_view<'a>(
        &'a self,
        session_id: &'a philo_session::SessionId,
    ) -> RuntimeFuture<'a, Result<SessionContextView, HostError>>;

    /// Rebuilds the model assembly for `/model`. The Idle gate is the
    /// host's responsibility: a Busy runtime must be refused, and a failed
    /// rebuild must keep the previous assembly.
    fn rebuild_model(&self, name: &str) -> Result<(), HostError>;

    /// Applies `/reasoning`. The runtime's reasoning effort is a per-turn
    /// input, so the new level takes effect from the next turn on; the host
    /// decides whether the active assembly supports it.
    fn set_reasoning(&self, effort: ReasoningEffort) -> Result<(), HostError>;

    /// The effective configuration with source layers (never secrets).
    fn config_view(&self) -> Vec<ConfigEntry>;

    /// The frozen tool lineup for `/status` (names and effect classes).
    fn tool_definitions(&self) -> Vec<ToolDefinition>;

    /// Mints a fresh session id for `/new` (id rules belong to the
    /// composition root, M9 decision 10).
    fn new_session_id(&self) -> String;

    /// The confirmation channel external approval decorators are wired to.
    /// An idle channel (nothing wired) never produces requests.
    fn confirmations(&self) -> ConfirmationChannel;
}
