//! Adapter from the CLI's prepared graph to the `philo-tui` host seam.

use std::sync::Arc;

use philo_agent_runtime::{
    AgentError, CompactionError, CompactionReport, OperationHandle, ReasoningEffort, RuntimeFuture,
    SessionId, ToolDefinition, UserMessage,
};
use philo_session::{SessionContextView, SessionStore};
use philo_session_jsonl::JsonlSessionStore;
use philo_tui::{ConfigEntry, ConfigReloadNotice, ConfirmationChannel, HostError, TuiHost};

use super::runtime_control::{ApplyError, ApplyResult, DisplayState, RuntimeControl};
use crate::assembly::RunAssembly;
use crate::config::{ResolveFlags, Settings, Verbosity};
use crate::error::UsageError;
use crate::ids::fresh_session_id;

pub struct CliHost {
    runtime: RuntimeControl,
    sessions: Arc<JsonlSessionStore>,
    confirmations: ConfirmationChannel,
}

impl CliHost {
    pub fn new(assembly: RunAssembly, flags: ResolveFlags) -> Self {
        let RunAssembly {
            settings,
            runtime_config,
            sessions,
            replay_store,
            model,
            ids,
            tools,
        } = assembly;
        let runtime = RuntimeControl::new(
            settings.deployment.clone(),
            runtime_config,
            model,
            replay_store,
            sessions.clone(),
            ids,
            tools,
            flags,
            &settings,
        );
        Self {
            runtime,
            sessions,
            confirmations: ConfirmationChannel::default(),
        }
    }

    pub fn on_reloaded_files(
        &self,
        result: Result<(Settings, Vec<String>), UsageError>,
    ) -> Option<ConfigReloadNotice> {
        match result {
            Err(error) => Some(ConfigReloadNotice::Failed {
                message: format!("config not reloaded: {}", error.0),
                clear_pending: false,
            }),
            Ok((settings, warnings)) => match self.runtime.apply_settings(settings) {
                Ok(ApplyResult::Unchanged) => None,
                Ok(ApplyResult::Applied {
                    display,
                    runtime_pending,
                }) => Some(applied_notice(display, runtime_pending, warnings)),
                Err(ApplyError::DataDir) => Some(ConfigReloadNotice::Failed {
                    message: "config not reloaded: data_dir cannot be changed without restarting"
                        .to_owned(),
                    clear_pending: false,
                }),
                Err(ApplyError::Assembly(message)) => Some(ConfigReloadNotice::Failed {
                    message: format!("config not reloaded: {message}"),
                    clear_pending: true,
                }),
            },
        }
    }

    pub fn on_watch_poll(&self) -> Option<ConfigReloadNotice> {
        match self.runtime.flush_pending() {
            Ok(None) | Ok(Some(ApplyResult::Unchanged)) => None,
            Ok(Some(ApplyResult::Applied {
                display,
                runtime_pending,
            })) => Some(applied_notice(display, runtime_pending, Vec::new())),
            Err(ApplyError::DataDir) => Some(ConfigReloadNotice::Failed {
                message: "config not reloaded: data_dir cannot be changed without restarting"
                    .to_owned(),
                clear_pending: false,
            }),
            Err(ApplyError::Assembly(message)) => Some(ConfigReloadNotice::Failed {
                message: format!("config not reloaded: {message}"),
                clear_pending: true,
            }),
        }
    }
}

fn applied_notice(
    display: DisplayState,
    runtime_pending: bool,
    warnings: Vec<String>,
) -> ConfigReloadNotice {
    let warnings = if display.verbosity == Verbosity::Quiet {
        Vec::new()
    } else {
        warnings
    };
    ConfigReloadNotice::Applied {
        show_reasoning: display.show_reasoning,
        verbose: display.verbosity == Verbosity::Verbose,
        context_window: display.context_window,
        model_name: display.model_name,
        runtime_pending,
        warnings,
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
        self.runtime
            .config_entries()
            .into_iter()
            .map(|entry| ConfigEntry {
                key: entry.key,
                value: entry.value,
                source: entry.source,
            })
            .collect()
    }

    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.runtime.tool_definitions()
    }

    fn new_session_id(&self) -> String {
        fresh_session_id()
    }

    fn confirmations(&self) -> ConfirmationChannel {
        self.confirmations.clone()
    }
}
