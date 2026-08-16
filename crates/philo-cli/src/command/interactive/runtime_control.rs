//! Runtime lifetime control for the interactive host.
//!
//! This module owns idle-only model rebuilding, config hot-reload apply,
//! and per-operation reasoning snapshots. The `TuiHost` adapter does not
//! need to know those invariants.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use philo_agent_runtime::{
    AgentAvailability, AgentError, AgentRuntime, CompactionConfig, CompactionError,
    CompactionReport, IdSource, ModelPort, OperationHandle, OperationId, ReasoningEffort,
    RuntimeConfig, RuntimeFuture, SessionId, ToolPort, UserMessage,
};
use philo_model::{
    ChatReasoningFormat, ModelCompat, ModelContinuationPolicy, ModelProtocol, ModelReplayStore,
};
use philo_session_jsonl::JsonlSessionStore;
use philo_tui::HostError;

use crate::config::{Deployment, EffectiveSetting, ResolveFlags, Settings, Verbosity};

struct Assembly {
    runtime: Arc<AgentRuntime>,
    config: RuntimeConfig,
    model: Arc<dyn ModelPort>,
    tools: Arc<dyn ToolPort>,
    deployment: Deployment,
}

#[derive(Default)]
struct ReasoningState {
    next: Option<ReasoningEffort>,
    operations: HashMap<OperationId, Option<ReasoningEffort>>,
}

/// Applies the reasoning value frozen when an operation was admitted. This
/// keeps queued operations stable when `/reasoning` changes again before
/// their model call begins.
struct ReasoningModel {
    inner: Arc<dyn ModelPort>,
    state: Arc<Mutex<ReasoningState>>,
}

impl ModelPort for ReasoningModel {
    fn start<'a>(
        &'a self,
        mut request: philo_agent_runtime::ModelCallSnapshot,
    ) -> RuntimeFuture<
        'a,
        Result<Box<dyn philo_agent_runtime::ModelEventStream>, philo_agent_runtime::ModelError>,
    > {
        if let Some(effort) = self
            .state
            .lock()
            .expect("reasoning state lock")
            .operations
            .get(&request.operation_id)
            .copied()
        {
            request.generation.reasoning_effort = effort;
        }
        self.inner.start(request)
    }
}

#[derive(Clone, Debug)]
struct AppliedSnapshot {
    show_reasoning: bool,
    verbosity: Verbosity,
    context_window: Option<u64>,
    max_tool_rounds: Option<u32>,
    max_parallel_tool_calls: Option<u32>,
    operation_timeout: Option<Duration>,
    shell_timeout_secs: Option<u64>,
    compaction: CompactionConfig,
    reasoning_effort: Option<ReasoningEffort>,
    model: String,
    endpoint: String,
    protocol: ModelProtocol,
    provider: String,
    api_key_env: String,
    compat: ModelCompat,
    chat_reasoning_format: Option<ChatReasoningFormat>,
    continuation_policy: ModelContinuationPolicy,
    header_names: Vec<String>,
    user_agent: Option<String>,
    entries: Vec<EffectiveSetting>,
}

impl AppliedSnapshot {
    fn from_settings(settings: &Settings) -> Self {
        Self {
            show_reasoning: settings.show_reasoning,
            verbosity: settings.verbosity,
            context_window: settings.context_window,
            max_tool_rounds: settings.max_tool_rounds,
            max_parallel_tool_calls: settings.max_parallel_tool_calls,
            operation_timeout: settings.operation_timeout,
            shell_timeout_secs: settings.shell_timeout_secs,
            compaction: settings.compaction.clone(),
            reasoning_effort: settings.reasoning_effort,
            model: settings.deployment.model.clone(),
            endpoint: settings.deployment.endpoint.clone(),
            protocol: settings.deployment.protocol,
            provider: settings.deployment.provider.clone(),
            api_key_env: settings.deployment.api_key_env.clone(),
            compat: settings.deployment.compat,
            chat_reasoning_format: settings.deployment.chat_reasoning_format,
            continuation_policy: settings.deployment.continuation_policy,
            header_names: settings
                .deployment
                .request_headers
                .names()
                .map(str::to_owned)
                .collect(),
            user_agent: settings
                .entries
                .iter()
                .find(|entry| entry.key == "header.user-agent")
                .map(|entry| entry.value.clone()),
            entries: settings.entries.clone(),
        }
    }

    fn display_eq(&self, other: &Self) -> bool {
        self.show_reasoning == other.show_reasoning
            && self.verbosity == other.verbosity
            && self.context_window == other.context_window
    }

    fn runtime_eq(&self, other: &Self) -> bool {
        self.max_tool_rounds == other.max_tool_rounds
            && self.max_parallel_tool_calls == other.max_parallel_tool_calls
            && self.operation_timeout == other.operation_timeout
            && self.shell_timeout_secs == other.shell_timeout_secs
            && self.compaction == other.compaction
            && self.reasoning_effort == other.reasoning_effort
    }

    fn deployment_eq(&self, other: &Self) -> bool {
        self.model == other.model
            && self.endpoint == other.endpoint
            && self.protocol == other.protocol
            && self.provider == other.provider
            && self.api_key_env == other.api_key_env
            && self.compat == other.compat
            && self.chat_reasoning_format == other.chat_reasoning_format
            && self.continuation_policy == other.continuation_policy
            && self.header_names == other.header_names
            && self.user_agent == other.user_agent
    }

    fn entries_eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

/// Visible TUI/config state after a successful parse is accepted.
#[derive(Clone, Debug)]
pub(super) struct DisplayState {
    pub show_reasoning: bool,
    pub verbosity: Verbosity,
    pub context_window: Option<u64>,
    pub model_name: String,
}

#[derive(Debug)]
pub(super) enum ApplyResult {
    Unchanged,
    Applied {
        display: DisplayState,
        runtime_pending: bool,
    },
}

#[derive(Debug)]
pub(super) enum ApplyError {
    DataDir,
    Assembly(String),
}

pub(super) struct RuntimeControl {
    assembly: Mutex<Assembly>,
    replay_store: Arc<dyn ModelReplayStore>,
    sessions: Arc<JsonlSessionStore>,
    ids: Arc<dyn IdSource>,
    reasoning: Arc<Mutex<ReasoningState>>,
    flags: ResolveFlags,
    data_dir: PathBuf,
    serving: Mutex<AppliedSnapshot>,
    pending: Mutex<Option<Settings>>,
}

impl RuntimeControl {
    pub fn new(
        deployment: Deployment,
        config: RuntimeConfig,
        model: Arc<dyn ModelPort>,
        replay_store: Arc<dyn ModelReplayStore>,
        sessions: Arc<JsonlSessionStore>,
        ids: Arc<dyn IdSource>,
        tools: Arc<dyn ToolPort>,
        flags: ResolveFlags,
        settings: &Settings,
    ) -> Self {
        let reasoning = Arc::new(Mutex::new(ReasoningState {
            next: config.generation.reasoning_effort,
            operations: HashMap::new(),
        }));
        let model: Arc<dyn ModelPort> = Arc::new(ReasoningModel {
            inner: model,
            state: reasoning.clone(),
        });
        let runtime = Arc::new(AgentRuntime::with_tools(
            model.clone(),
            sessions.clone(),
            ids.clone(),
            config.clone(),
            tools.clone(),
        ));
        Self {
            assembly: Mutex::new(Assembly {
                runtime,
                config,
                model,
                tools,
                deployment,
            }),
            replay_store,
            sessions,
            ids,
            reasoning,
            flags,
            data_dir: settings.data_dir.clone(),
            serving: Mutex::new(AppliedSnapshot::from_settings(settings)),
            pending: Mutex::new(None),
        }
    }

    pub fn prompt<'a>(
        &'a self,
        session_id: SessionId,
        message: UserMessage,
    ) -> RuntimeFuture<'a, Result<OperationHandle, AgentError>> {
        let runtime = self.runtime();
        let starts_new_run = matches!(runtime.availability(), AgentAvailability::Idle);
        let reasoning = self.reasoning.clone();
        Box::pin(async move {
            let handle = runtime.prompt(session_id, message).await?;
            let mut state = reasoning.lock().expect("reasoning state lock");
            if starts_new_run {
                state.operations.clear();
            }
            let effort = state.next;
            state
                .operations
                .insert(handle.operation_id().clone(), effort);
            Ok(handle)
        })
    }

    pub fn rebuild_model(&self, name: &str) -> Result<(), HostError> {
        // Build first: a rejected name must not disturb the serving assembly.
        let deployment = self
            .assembly
            .lock()
            .expect("host assembly lock")
            .deployment
            .clone();
        let adapter = crate::assembly::build_model(&deployment, name, self.replay_store.clone())
            .map_err(|error| HostError::new(format!("{error}")))?;
        let model: Arc<dyn ModelPort> = Arc::new(ReasoningModel {
            inner: Arc::new(adapter),
            state: self.reasoning.clone(),
        });
        self.rebuild(Some(model), None, None, |config| {
            config.model_target = name.to_owned();
        })
    }

    pub fn set_reasoning(&self, effort: ReasoningEffort) {
        self.reasoning.lock().expect("reasoning state lock").next = Some(effort);
    }

    pub fn compact(
        &self,
        session_id: SessionId,
    ) -> RuntimeFuture<'static, Result<CompactionReport, CompactionError>> {
        let runtime = self.runtime();
        Box::pin(async move { runtime.compact(session_id).await })
    }

    pub fn tool_definitions(&self) -> Vec<philo_agent_runtime::ToolDefinition> {
        self.assembly
            .lock()
            .expect("host assembly lock")
            .tools
            .definitions()
    }

    pub fn config_entries(&self) -> Vec<EffectiveSetting> {
        self.serving
            .lock()
            .expect("serving snapshot lock")
            .entries
            .clone()
    }

    pub fn availability(&self) -> AgentAvailability {
        self.runtime().availability()
    }

    pub fn apply_settings(&self, settings: Settings) -> Result<ApplyResult, ApplyError> {
        if settings.data_dir != self.data_dir {
            return Err(ApplyError::DataDir);
        }
        let incoming = AppliedSnapshot::from_settings(&settings);
        let baseline = {
            let pending = self.pending.lock().expect("pending reload lock");
            pending
                .as_ref()
                .map(AppliedSnapshot::from_settings)
                .unwrap_or_else(|| self.serving.lock().expect("serving snapshot lock").clone())
        };
        if incoming.display_eq(&baseline)
            && incoming.runtime_eq(&baseline)
            && incoming.deployment_eq(&baseline)
            && incoming.entries_eq(&baseline)
        {
            return Ok(ApplyResult::Unchanged);
        }

        let display_changed = !incoming.display_eq(&baseline) || !incoming.entries_eq(&baseline);
        let rebuild_runtime = !incoming.runtime_eq(&baseline);
        let rebuild_tools = incoming.shell_timeout_secs != baseline.shell_timeout_secs;
        let rebuild_model = !incoming.deployment_eq(&baseline);
        let needs_runtime = rebuild_runtime || rebuild_tools || rebuild_model;

        if display_changed && !needs_runtime {
            self.apply_display(&incoming);
            return Ok(ApplyResult::Applied {
                display: self.display_state(),
                runtime_pending: self.pending.lock().expect("pending reload lock").is_some(),
            });
        }

        if !needs_runtime {
            return Ok(ApplyResult::Unchanged);
        }

        if !self.is_idle() {
            if display_changed {
                self.apply_display(&incoming);
            }
            *self.pending.lock().expect("pending reload lock") = Some(settings);
            return Ok(ApplyResult::Applied {
                display: self.display_state(),
                runtime_pending: true,
            });
        }

        if !self.swap_from_settings(settings, rebuild_model, rebuild_tools, rebuild_runtime)? {
            return Ok(ApplyResult::Applied {
                display: self.display_state(),
                runtime_pending: true,
            });
        }
        *self.pending.lock().expect("pending reload lock") = None;
        Ok(ApplyResult::Applied {
            display: self.display_state(),
            runtime_pending: false,
        })
    }

    pub fn flush_pending(&self) -> Result<Option<ApplyResult>, ApplyError> {
        if !self.is_idle() {
            return Ok(None);
        }
        let Some(settings) = self.pending.lock().expect("pending reload lock").take() else {
            return Ok(None);
        };
        if settings.data_dir != self.data_dir {
            return Err(ApplyError::DataDir);
        }
        let incoming = AppliedSnapshot::from_settings(&settings);
        let serving = self.serving.lock().expect("serving snapshot lock").clone();
        let rebuild_runtime = !incoming.runtime_eq(&serving);
        let rebuild_tools = incoming.shell_timeout_secs != serving.shell_timeout_secs;
        let rebuild_model = !incoming.deployment_eq(&serving);
        if !rebuild_runtime && !rebuild_tools && !rebuild_model {
            self.apply_display(&incoming);
            return Ok(Some(ApplyResult::Applied {
                display: self.display_state(),
                runtime_pending: false,
            }));
        }
        let applied =
            self.swap_from_settings(settings, rebuild_model, rebuild_tools, rebuild_runtime)?;
        if !applied {
            return Ok(Some(ApplyResult::Applied {
                display: self.display_state(),
                runtime_pending: true,
            }));
        }
        Ok(Some(ApplyResult::Applied {
            display: self.display_state(),
            runtime_pending: false,
        }))
    }

    fn apply_display(&self, incoming: &AppliedSnapshot) {
        let mut serving = self.serving.lock().expect("serving snapshot lock");
        serving.show_reasoning = incoming.show_reasoning;
        serving.verbosity = incoming.verbosity;
        serving.context_window = incoming.context_window;
        merge_display_entries(&mut serving.entries, &incoming.entries);
    }

    fn display_state(&self) -> DisplayState {
        let serving = self.serving.lock().expect("serving snapshot lock");
        let model_name = self
            .assembly
            .lock()
            .expect("host assembly lock")
            .config
            .model_target
            .clone();
        DisplayState {
            show_reasoning: serving.show_reasoning,
            verbosity: serving.verbosity,
            context_window: serving.context_window,
            model_name,
        }
    }

    fn swap_from_settings(
        &self,
        settings: Settings,
        rebuild_model: bool,
        rebuild_tools: bool,
        rebuild_runtime: bool,
    ) -> Result<bool, ApplyError> {
        let cli = self.flags.to_cli();
        let model = if rebuild_model {
            let adapter = crate::assembly::build_model(
                &settings.deployment,
                &settings.deployment.model,
                self.replay_store.clone(),
            )
            .map_err(|error| ApplyError::Assembly(error.to_string()))?;
            Some(Arc::new(ReasoningModel {
                inner: Arc::new(adapter),
                state: self.reasoning.clone(),
            }) as Arc<dyn ModelPort>)
        } else {
            None
        };
        let tools = if rebuild_tools {
            Some(
                crate::assembly::tool_port_for(&settings)
                    .map_err(|error| ApplyError::Assembly(error.0))?,
            )
        } else {
            None
        };
        let model_target = if rebuild_model {
            settings.deployment.model.clone()
        } else {
            self.assembly
                .lock()
                .expect("host assembly lock")
                .config
                .model_target
                .clone()
        };
        let config = if rebuild_runtime || rebuild_model {
            Some(
                crate::assembly::runtime_config_for(&cli, &settings, &model_target)
                    .map_err(|error| ApplyError::Assembly(error.0))?,
            )
        } else {
            None
        };
        let snapshot = AppliedSnapshot::from_settings(&settings);
        match self.rebuild(model, tools, config, |_| {}) {
            Ok(()) => {}
            Err(error)
                if error.message().contains("still running")
                    || error.message().contains("compaction is still running") =>
            {
                *self.pending.lock().expect("pending reload lock") = Some(settings);
                return Ok(false);
            }
            Err(error) => return Err(ApplyError::Assembly(error.message().to_owned())),
        }
        if rebuild_model {
            self.assembly.lock().expect("host assembly lock").deployment = settings.deployment;
        }
        if rebuild_runtime || rebuild_model {
            let next = self
                .assembly
                .lock()
                .expect("host assembly lock")
                .config
                .generation
                .reasoning_effort;
            self.reasoning.lock().expect("reasoning state lock").next = next;
        }
        *self.serving.lock().expect("serving snapshot lock") = snapshot;
        Ok(true)
    }

    fn is_idle(&self) -> bool {
        matches!(self.availability(), AgentAvailability::Idle)
    }

    fn runtime(&self) -> Arc<AgentRuntime> {
        self.assembly
            .lock()
            .expect("host assembly lock")
            .runtime
            .clone()
    }

    /// Replaces the runtime only while idle: a live scheduler must never be
    /// swapped out from under a running operation.
    fn rebuild(
        &self,
        model: Option<Arc<dyn ModelPort>>,
        tools: Option<Arc<dyn ToolPort>>,
        config: Option<RuntimeConfig>,
        mutate: impl FnOnce(&mut RuntimeConfig),
    ) -> Result<(), HostError> {
        let mut assembly = self.assembly.lock().expect("host assembly lock");
        match assembly.runtime.availability() {
            AgentAvailability::Idle => {}
            AgentAvailability::Busy { .. } => {
                return Err(HostError::new("a turn is still running"));
            }
            AgentAvailability::Compacting { .. } => {
                return Err(HostError::new("context compaction is still running"));
            }
        }
        let mut next_config = config.unwrap_or_else(|| assembly.config.clone());
        mutate(&mut next_config);
        let model = model.unwrap_or_else(|| assembly.model.clone());
        let tools = tools.unwrap_or_else(|| assembly.tools.clone());
        assembly.runtime = Arc::new(AgentRuntime::with_tools(
            model.clone(),
            self.sessions.clone(),
            self.ids.clone(),
            next_config.clone(),
            tools.clone(),
        ));
        assembly.config = next_config;
        assembly.model = model;
        assembly.tools = tools;
        Ok(())
    }
}

fn merge_display_entries(current: &mut Vec<EffectiveSetting>, incoming: &[EffectiveSetting]) {
    for key in ["verbosity", "show_reasoning", "context_window"] {
        if let Some(entry) = incoming.iter().find(|entry| entry.key == key) {
            if let Some(slot) = current.iter_mut().find(|entry| entry.key == key) {
                *slot = entry.clone();
            } else {
                current.push(entry.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests;
