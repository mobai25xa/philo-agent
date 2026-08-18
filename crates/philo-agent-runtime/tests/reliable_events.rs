//! Reliable lane backpressure: facts are not dropped when the subscription is full.

mod support;

use std::sync::Arc;
use std::time::Duration;

use philo_agent_runtime::{
    AgentEvent, ChannelBounds, GenerationConfig, OperationStatus, RuntimeConfig, RuntimeEvent,
    SequentialIdSource, SessionId, SettlementDurability, TryRecvError, UserMessage,
};
use philo_session::MemorySessionStore;
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};
use support::gate::Gate;
use support::runtime::{
    empty_tools, event_cap_bounds, generation, start_with_bounds, submit_prompt, wait_until_idle,
};

fn config() -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "sys".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds: 1,
        max_parallel_tool_calls: 1,
        operation_timeout: None,
        tool_cancel_grace: Duration::from_millis(300),
        compaction: Default::default(),
    }
}

fn echo() -> philo_agent_runtime::ToolDefinition {
    philo_agent_runtime::ToolDefinition::simple(
        "echo",
        "echo",
        philo_agent_runtime::EffectClass::ReadOnly,
    )
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

async fn drain_after_idle(
    handle: &philo_agent_runtime::RuntimeHandle,
    sub: &mut philo_agent_runtime::RuntimeEventReceiver,
) -> Vec<AgentEvent> {
    wait_until_idle(handle).await;
    drain_agent_events(sub, "timed out before reliable settlement arrived").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reliable_facts_arrive_in_order_when_event_cap_is_one() {
    let model = Arc::new(FakeModel::succeeds(&["hello"]));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, mut sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        event_cap_bounds(1),
    )
    .await;
    let accepted = submit_prompt(
        &handle,
        generation(model, empty_tools(), config()),
        "session",
        "hi",
    )
    .await;
    let events = drain_after_idle(&handle, &mut sub).await;
    let started = events
        .iter()
        .position(|event| matches!(event, AgentEvent::OperationStarted { .. }))
        .expect("started");
    let completed = events
        .iter()
        .position(|event| matches!(event, AgentEvent::AssistantMessageCompleted { .. }))
        .expect("assistant completed");
    let settled = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::OperationSettled {
                    operation_id,
                    status: OperationStatus::Succeeded,
                    durability: SettlementDurability::Confirmed,
                    ..
                } if operation_id == &accepted.operation_id
            )
        })
        .expect("settled");
    assert!(started < completed && completed < settled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_progress_coalesces_under_cap_one() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::text(&["done"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let chunks = std::iter::repeat("x").take(20_000);
    let tools = Arc::new(FakeTool::one(
        echo(),
        FakeToolResult::streaming_success(chunks, "ok"),
    ));
    let (handle, mut sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        event_cap_bounds(1),
    )
    .await;
    let _ = handle
        .submit(philo_agent_runtime::OperationSpec {
            session_id: SessionId::new("s"),
            user_message: UserMessage::new("hi"),
            generation: generation(model, tools, config()),
            service_request_id: None,
        })
        .await
        .unwrap();
    let events = drain_after_idle(&handle, &mut sub).await;
    let started = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ToolExecutionStarted { .. }))
        .expect("tool started");
    let completed = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ToolExecutionCompleted { .. }))
        .expect("tool completed");
    let settled = events
        .iter()
        .position(|event| matches!(event, AgentEvent::OperationSettled { .. }))
        .expect("settled");
    let progress: Vec<_> = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::ToolExecutionProgress { .. }))
        .collect();
    assert!(started < completed && completed < settled);
    assert!(
        progress.len() <= 2,
        "progress must coalesce, got {}",
        progress.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pausing_the_consumer_does_not_drop_settlement() {
    let model = Arc::new(FakeModel::succeeds(&["hello"]));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, mut sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        ChannelBounds {
            command_cap: 8,
            control_cap: 8,
            event_cap: 2,
            queue_max: 8,
            driver_event_budget: 8,
            reliable_staging_cap: 64,
        },
    )
    .await;
    submit_prompt(
        &handle,
        generation(model, empty_tools(), config()),
        "session",
        "hi",
    )
    .await;
    wait_until_idle(&handle).await;
    assert!(matches!(sub.try_recv(), Ok(_) | Err(TryRecvError::Empty)));
    let events = drain_after_idle(&handle, &mut sub).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::AssistantMessageCompleted { .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::OperationSettled { .. }))
    );
}

/// 04 matrix: capacity 1 + consumer gate. The drain task is parked until the
/// operation reaches Idle on the snapshot path; the subscription stays unread
/// while reliable facts queue. Releasing the gate must still yield ordered
/// facts and coalesced transients — no `sleep`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gated_consumer_sees_ordered_facts_after_event_cap_one() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::text(&["done"]),
    ]));
    let chunks = std::iter::repeat("x").take(512);
    let tools = Arc::new(FakeTool::one(
        echo(),
        FakeToolResult::streaming_success(chunks, "ok"),
    ));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, mut sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        event_cap_bounds(1),
    )
    .await;
    let accepted = handle
        .submit(philo_agent_runtime::OperationSpec {
            session_id: SessionId::new("s"),
            user_message: UserMessage::new("hi"),
            generation: generation(model, tools, config()),
            service_request_id: None,
        })
        .await
        .unwrap();

    let gate = Gate::new();
    let drain = {
        let gate = gate.clone();
        async move {
            gate.wait().await;
            drain_after_idle_recv(&mut sub).await
        }
    };
    let drain_task = tokio::spawn(drain);

    wait_until_idle(&handle).await;
    assert!(
        !gate.is_released(),
        "consumer must still be gated while the runtime is already idle"
    );
    gate.release();
    let events = drain_task.await.expect("drain task");

    let started = events
        .iter()
        .position(|event| matches!(event, AgentEvent::OperationStarted { .. }))
        .expect("started");
    let tool_started = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ToolExecutionStarted { .. }))
        .expect("tool started");
    let tool_completed = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ToolExecutionCompleted { .. }))
        .expect("tool completed");
    let assistant = events
        .iter()
        .position(|event| matches!(event, AgentEvent::AssistantMessageCompleted { .. }))
        .expect("assistant completed");
    let settled = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::OperationSettled {
                    operation_id,
                    status: OperationStatus::Succeeded,
                    ..
                } if operation_id == &accepted.operation_id
            )
        })
        .expect("settled");
    let progress = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::ToolExecutionProgress { .. }))
        .count();
    assert!(started < tool_started);
    assert!(tool_started < tool_completed);
    assert!(tool_completed < assistant);
    assert!(assistant < settled);
    assert!(
        progress <= 2,
        "progress must coalesce under a gated consumer, got {progress}"
    );
}

async fn drain_after_idle_recv(
    sub: &mut philo_agent_runtime::RuntimeEventReceiver,
) -> Vec<AgentEvent> {
    drain_agent_events(
        sub,
        "gated consumer timed out before reliable settlement arrived",
    )
    .await
}

async fn drain_agent_events(
    sub: &mut philo_agent_runtime::RuntimeEventReceiver,
    timeout_message: &str,
) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    let mut saw_settled = false;
    loop {
        match tokio::time::timeout(Duration::from_millis(250), sub.recv()).await {
            Ok(Some(event)) => {
                if let Some(agent) = into_agent_event(event) {
                    if matches!(agent, AgentEvent::OperationSettled { .. }) {
                        saw_settled = true;
                    }
                    events.push(agent);
                }
            }
            Ok(None) => break,
            Err(_) if saw_settled => break,
            Err(_) => panic!("{timeout_message}"),
        }
    }
    events
}
