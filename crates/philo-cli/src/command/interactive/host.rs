//! Adapter from the CLI's prepared graph to the `philo-tui` host seam.

use std::sync::Arc;

use philo_agent_runtime::{
    AgentError, CompactionError, CompactionReport, OperationHandle, ReasoningEffort, RuntimeFuture,
    SessionId, ToolDefinition, UserMessage,
};
use philo_session::{SessionContextView, SessionStore};
use philo_session_jsonl::JsonlSessionStore;
use philo_tui::{ConfigEntry, ConfirmationChannel, HostError, TuiHost};

use super::runtime_control::RuntimeControl;
use crate::assembly::RunAssembly;
use crate::config::Settings;
use crate::ids::fresh_session_id;

pub struct CliHost {
    runtime: RuntimeControl,
    sessions: Arc<JsonlSessionStore>,
    tools: Arc<dyn philo_agent_runtime::ToolPort>,
    entries: Vec<crate::config::EffectiveSetting>,
    confirmations: ConfirmationChannel,
}

impl CliHost {
    pub fn new(assembly: RunAssembly) -> Self {
        let RunAssembly {
            settings,
            runtime_config,
            sessions,
            model,
            ids,
            tools,
        } = assembly;
        let Settings {
            deployment,
            entries,
            ..
        } = settings;
        let runtime = RuntimeControl::new(
            deployment,
            runtime_config,
            model,
            sessions.clone(),
            ids,
            tools.clone(),
        );
        Self {
            runtime,
            sessions,
            tools,
            entries,
            confirmations: ConfirmationChannel::default(),
        }
    }
}

impl TuiHost for CliHost {
    fn prompt<'a>(
        &'a self,
        session_id: SessionId,
        message: UserMessage,
    ) -> RuntimeFuture<'a, Result<OperationHandle, AgentError>> {
        self.runtime.prompt(session_id, message)
    }

    fn compact(
        &self,
        session_id: SessionId,
    ) -> RuntimeFuture<'static, Result<CompactionReport, CompactionError>> {
        self.runtime.compact(session_id)
    }

    fn list_sessions(&self) -> Result<Vec<philo_session::SessionId>, HostError> {
        self.sessions
            .list_sessions()
            .map_err(|error| HostError::new(error.to_string()))
    }

    fn context_view<'a>(
        &'a self,
        session_id: &'a philo_session::SessionId,
    ) -> RuntimeFuture<'a, Result<SessionContextView, HostError>> {
        let sessions = self.sessions.clone();
        Box::pin(async move {
            sessions
                .context_view(session_id)
                .await
                .map_err(|error| HostError::new(format!("{error:?}")))
        })
    }

    fn rebuild_model(&self, name: &str) -> Result<(), HostError> {
        self.runtime.rebuild_model(name)
    }

    fn set_reasoning(&self, effort: ReasoningEffort) -> Result<(), HostError> {
        self.runtime.set_reasoning(effort);
        Ok(())
    }

    fn config_view(&self) -> Vec<ConfigEntry> {
        self.entries
            .iter()
            .map(|entry| ConfigEntry {
                key: entry.key.clone(),
                value: entry.value.clone(),
                source: entry.source.clone(),
            })
            .collect()
    }

    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tools.definitions()
    }

    fn new_session_id(&self) -> String {
        fresh_session_id()
    }

    fn confirmations(&self) -> ConfirmationChannel {
        self.confirmations.clone()
    }
}
