//! Shared dependency assembly for single-shot and interactive commands.
//!
//! Both modes consume the same prepared graph; only their presentation and
//! lifetime differ. Keeping this path singular prevents configuration drift.

use std::sync::Arc;

use philo_agent_runtime::{IdSource, ModelPort, RuntimeConfig, ToolPort};
use philo_coding_profile::CodingProfile;
use philo_model::{AdapterBuildError, FileModelReplayStore, ModelReplayStore, PhiloModelAdapter};
use philo_session_jsonl::JsonlSessionStore;

use crate::args::Cli;
use crate::config::Settings;
use crate::error::UsageError;
use crate::ids::ProcessIdSource;

/// The fully prepared dependency graph shared by both execution modes.
pub struct RunAssembly {
    pub settings: Settings,
    pub runtime_config: RuntimeConfig,
    pub sessions: Arc<JsonlSessionStore>,
    pub replay_store: Arc<dyn ModelReplayStore>,
    pub model: Arc<dyn ModelPort>,
    pub ids: Arc<dyn IdSource>,
    pub tools: Arc<dyn ToolPort>,
}

impl RunAssembly {
    /// Builds the profile, runtime configuration, persistence adapter, model
    /// adapter, ID source, and tool registry exactly once.
    pub fn prepare(cli: &Cli, settings: Settings) -> Result<Self, UsageError> {
        let workspace_root = std::env::current_dir().map_err(|error| {
            UsageError::new(format!("cannot resolve the working directory: {error}"))
        })?;
        let mut profile = CodingProfile::new(workspace_root);
        if let Some(seconds) = settings.shell_timeout_secs {
            profile = profile.with_shell_timeout_secs(seconds);
        }

        let mut runtime_config = profile.runtime_config(&settings.deployment.model);
        if let Some(system) = &cli.system {
            runtime_config.system_prompt = system.clone();
        }
        if let Some(rounds) = settings.max_tool_rounds {
            runtime_config.max_tool_rounds = rounds;
        }
        if let Some(parallel) = settings.max_parallel_tool_calls {
            runtime_config.max_parallel_tool_calls = parallel;
        }
        if settings.reasoning_effort.is_some() {
            runtime_config.generation.reasoning_effort = settings.reasoning_effort;
        }
        runtime_config.operation_timeout = settings.operation_timeout;
        runtime_config.compaction = settings.compaction.clone();

        let sessions = Arc::new(
            JsonlSessionStore::open(&settings.data_dir).map_err(|error| {
                UsageError::new(format!("cannot open the session store: {error}"))
            })?,
        );
        let replay_store: Arc<dyn ModelReplayStore> = Arc::new(
            FileModelReplayStore::open(&settings.data_dir).map_err(|error| {
                UsageError::new(format!("cannot open the model replay sidecar: {error}"))
            })?,
        );
        let model: Arc<dyn ModelPort> = Arc::new(
            build_model(
                &settings.deployment,
                &settings.deployment.model,
                replay_store.clone(),
            )
            .map_err(|error| UsageError::new(format!("model assembly failed: {error}")))?,
        );
        let ids: Arc<dyn IdSource> = Arc::new(ProcessIdSource::new());
        let tools: Arc<dyn ToolPort> = Arc::new(profile.tool_registry());

        Ok(Self {
            settings,
            runtime_config,
            sessions,
            replay_store,
            model,
            ids,
            tools,
        })
    }
}

/// One model construction path shared by startup and interactive `/model`
/// rebuilding, so deployment headers and credentials cannot drift.
pub(crate) fn build_model(
    deployment: &crate::config::Deployment,
    model: &str,
    replay_store: Arc<dyn ModelReplayStore>,
) -> Result<PhiloModelAdapter, AdapterBuildError> {
    let mut builder = PhiloModelAdapter::builder(
        deployment.provider.clone(),
        deployment.protocol,
        model,
        deployment.endpoint.clone(),
    )
    .api_key_env(&deployment.api_key_env)
    .request_headers(deployment.request_headers.clone())
    .replay_store(replay_store)
    .compat(deployment.compat)
    .continuation_policy(deployment.continuation_policy);
    if let Some(format) = deployment.chat_reasoning_format {
        builder = builder.chat_reasoning_format(format);
    }
    builder.build()
}

/// Maps resolved settings onto a RuntimeConfig the same way [`RunAssembly::prepare`]
/// does, without opening stores or building a model.
pub(crate) fn runtime_config_for(
    cli: &Cli,
    settings: &Settings,
    model_target: &str,
) -> Result<RuntimeConfig, UsageError> {
    let workspace_root = std::env::current_dir().map_err(|error| {
        UsageError::new(format!("cannot resolve the working directory: {error}"))
    })?;
    let mut profile = CodingProfile::new(workspace_root);
    if let Some(seconds) = settings.shell_timeout_secs {
        profile = profile.with_shell_timeout_secs(seconds);
    }
    let mut runtime_config = profile.runtime_config(model_target);
    if let Some(system) = &cli.system {
        runtime_config.system_prompt = system.clone();
    }
    if let Some(rounds) = settings.max_tool_rounds {
        runtime_config.max_tool_rounds = rounds;
    }
    if let Some(parallel) = settings.max_parallel_tool_calls {
        runtime_config.max_parallel_tool_calls = parallel;
    }
    if settings.reasoning_effort.is_some() {
        runtime_config.generation.reasoning_effort = settings.reasoning_effort;
    }
    runtime_config.operation_timeout = settings.operation_timeout;
    runtime_config.compaction = settings.compaction.clone();
    Ok(runtime_config)
}

/// Rebuilds the coding ToolPort from resolved settings (shell timeout).
pub(crate) fn tool_port_for(settings: &Settings) -> Result<Arc<dyn ToolPort>, UsageError> {
    let workspace_root = std::env::current_dir().map_err(|error| {
        UsageError::new(format!("cannot resolve the working directory: {error}"))
    })?;
    let mut profile = CodingProfile::new(workspace_root);
    if let Some(seconds) = settings.shell_timeout_secs {
        profile = profile.with_shell_timeout_secs(seconds);
    }
    Ok(Arc::new(profile.tool_registry()))
}
