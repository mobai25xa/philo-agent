//! Runtime lifetime control for the interactive host.
//!
//! This module owns idle-only model rebuilding and per-operation reasoning
//! snapshots. The `TuiHost` adapter does not need to know those invariants.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use philo_agent_runtime::{
    AgentAvailability, AgentError, AgentRuntime, CompactionError, CompactionReport, IdSource,
    ModelPort, OperationHandle, OperationId, ReasoningEffort, RuntimeConfig, RuntimeFuture,
    SessionId, ToolPort, UserMessage,
};
use philo_model::ModelReplayStore;
use philo_session_jsonl::JsonlSessionStore;
use philo_tui::HostError;

use crate::config::Deployment;

struct Assembly {
    runtime: Arc<AgentRuntime>,
    config: RuntimeConfig,
    model: Arc<dyn ModelPort>,
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

pub(super) struct RuntimeControl {
    assembly: Mutex<Assembly>,
    deployment: Deployment,
    replay_store: Arc<dyn ModelReplayStore>,
    sessions: Arc<JsonlSessionStore>,
    ids: Arc<dyn IdSource>,
    tools: Arc<dyn ToolPort>,
    reasoning: Arc<Mutex<ReasoningState>>,
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
            }),
            deployment,
            replay_store,
            sessions,
            ids,
            tools,
            reasoning,
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
        let adapter =
            crate::assembly::build_model(&self.deployment, name, self.replay_store.clone())
                .map_err(|error| HostError::new(format!("{error}")))?;
        let model: Arc<dyn ModelPort> = Arc::new(ReasoningModel {
            inner: Arc::new(adapter),
            state: self.reasoning.clone(),
        });
        self.rebuild(Some(model), |config| {
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
        let mut config = assembly.config.clone();
        mutate(&mut config);
        let model = model.unwrap_or_else(|| assembly.model.clone());
        assembly.runtime = Arc::new(AgentRuntime::with_tools(
            model.clone(),
            self.sessions.clone(),
            self.ids.clone(),
            config.clone(),
            self.tools.clone(),
        ));
        assembly.config = config;
        assembly.model = model;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
