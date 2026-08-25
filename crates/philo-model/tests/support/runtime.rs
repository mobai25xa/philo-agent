//! Shared helpers for driving the self-driven runtime from model tests.
//!
//! Copied from `philo-agent-runtime/tests/support/runtime.rs` (cannot import
//! another crate's `tests/`).

use std::sync::Arc;

use philo_agent_runtime::{
    AgentEvent, AgentFailure, AgentRuntime, ChannelBounds, FailureDomain, FailureStage, GenerationDisplay,
    GenerationId, IdSource, ModelPort, OperationId, OperationOutcome, OperationSpec,
    OperationStatus, RuntimeConfig, RuntimeDeps, RuntimeEvent, RuntimeEventReceiver,
    RuntimeGeneration, RuntimeHandle, SequentialIdSource, SessionId, SettlementDurability,
    ToolPort, ToolRegistry, UserMessage,
};
use philo_session::SessionStore;

pub fn empty_tools() -> Arc<dyn ToolPort> {
    Arc::new(ToolRegistry::empty())
}

pub fn generation(
    model: Arc<dyn ModelPort>,
    tools: Arc<dyn ToolPort>,
    config: RuntimeConfig,
) -> Arc<RuntimeGeneration> {
    let model_name = config.model_target.clone();
    Arc::new(RuntimeGeneration {
        generation_id: GenerationId::new("test-generation"),
        model,
        tools,
        runtime_config: config,
        display: GenerationDisplay { model_name, image_input: true },
    })
}

/// Alias used by replay tests that already call [`generation`].
pub fn test_generation(
    model: impl ModelPort + 'static,
    tools: Arc<dyn ToolPort>,
    runtime_config: RuntimeConfig,
) -> Arc<RuntimeGeneration> {
    generation(Arc::new(model), tools, runtime_config)
}

pub async fn start(
    _model: Arc<dyn ModelPort>,
    sessions: Arc<dyn SessionStore>,
    _tools: Arc<dyn ToolPort>,
    _config: RuntimeConfig,
) -> (RuntimeHandle, RuntimeEventReceiver) {
    start_with_ids(sessions, Arc::new(SequentialIdSource::new()))
}

pub fn start_with_ids(
    sessions: Arc<dyn SessionStore>,
    ids: Arc<dyn IdSource>,
) -> (RuntimeHandle, RuntimeEventReceiver) {
    let parts = AgentRuntime::start(RuntimeDeps {
        sessions,
        ids,
        bounds: ChannelBounds::default(),
    })
    .expect("start runtime");
    (parts.handle, parts.events)
}

pub fn start_runtime(
    sessions: Arc<dyn SessionStore>,
    ids: Arc<dyn IdSource>,
) -> (RuntimeHandle, RuntimeEventReceiver) {
    start_with_ids(sessions, ids)
}

pub async fn submit_prompt(
    handle: &RuntimeHandle,
    session_id: SessionId,
    user_message: UserMessage,
    generation: Arc<RuntimeGeneration>,
) -> OperationId {
    handle
        .submit(OperationSpec {
            session_id,
            user_message,
            generation,
            service_request_id: None,
        })
        .await
        .expect("submit accepted")
        .operation_id
}

/// Collects AgentEvents for `operation_id` until it settles. Non-agent
/// runtime events are skipped. Sibling operation events are also skipped.
pub async fn drain_until_settled(
    sub: &mut RuntimeEventReceiver,
    operation_id: &OperationId,
) -> (Vec<AgentEvent>, OperationOutcome) {
    let mut events = Vec::new();
    let mut started = false;
    let mut assistant = None;
    let mut failure = None;
    loop {
        let Some(event) = sub.recv().await else {
            panic!("subscription closed before {operation_id} settled");
        };
        let Some(agent) = into_agent_event(event) else {
            continue;
        };
        if !belongs(&agent, operation_id, started) {
            continue;
        }
        if matches!(&agent, AgentEvent::OperationStarted { .. }) {
            started = true;
        }
        match &agent {
            AgentEvent::AssistantMessageCompleted { message, .. } => {
                assistant = Some(message.clone());
            }
            AgentEvent::TurnFailed { failure: next, .. } => {
                failure = Some(next.clone());
            }
            _ => {}
        }
        let settled = match &agent {
            AgentEvent::OperationSettled {
                operation_id: id,
                status,
                durability,
                ..
            } if id == operation_id => Some((*status, *durability)),
            _ => None,
        };
        events.push(agent);
        if let Some((status, durability)) = settled {
            return (events, outcome_from(status, durability, assistant, failure));
        }
    }
}

fn into_agent_event(event: RuntimeEvent) -> Option<AgentEvent> {
    match event {
        RuntimeEvent::Agent(agent) => Some(agent),
        RuntimeEvent::OperationSettled {
            operation_id,
            status,
            durability,
            session_revision,
            ..
        } => Some(AgentEvent::OperationSettled {
            operation_id,
            status,
            durability,
            session_revision,
        }),
        _ => None,
    }
}

fn belongs(event: &AgentEvent, operation_id: &OperationId, started: bool) -> bool {
    match event {
        AgentEvent::OperationQueued { operation_id: id }
        | AgentEvent::OperationStarted { operation_id: id }
        | AgentEvent::CancellationRequested {
            operation_id: id, ..
        }
        | AgentEvent::OperationSettled {
            operation_id: id, ..
        } => id == operation_id,
        _ => started,
    }
}

fn outcome_from(
    status: OperationStatus,
    durability: SettlementDurability,
    assistant: Option<philo_agent_runtime::AssistantMessage>,
    failure: Option<AgentFailure>,
) -> OperationOutcome {
    match status {
        OperationStatus::Succeeded => OperationOutcome::Succeeded {
            assistant: assistant.expect("succeeded operation published assistant message"),
        },
        OperationStatus::Cancelled => OperationOutcome::Cancelled,
        OperationStatus::Failed => OperationOutcome::Failed {
            failure: failure.unwrap_or_else(|| {
                AgentFailure::new(
                    "engine.invariant_violation",
                    FailureDomain::Internal,
                    FailureStage::TurnEngine,
                    philo_agent_runtime::RetryDisposition::Never,
                    "an internal driver invariant was violated",
                    match durability {
                        SettlementDurability::Unconfirmed => "unconfirmed failure",
                        SettlementDurability::Confirmed => "failed without TurnFailed",
                    },
                )
            }),
            durability,
        },
    }
}
