mod support;

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use philo_agent_runtime::{
    AgentEvent, AgentRuntime, GenerationConfig, OperationOutcome, RuntimeConfig,
    SequentialIdSource, SessionId, ToolDefinition, UserMessage,
};
use philo_session::{ContextMessage, MemorySessionStore, SessionStore};
use support::failing_session::{FailingSessionStore, FailurePlan};
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
    }
}

fn config() -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "sys".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        // M2 semantics are the max_tool_rounds = 1 special case of the M3 loop.
        max_tool_rounds: 1,
        operation_timeout: None,
        compaction: Default::default(),
    }
}

fn echo_definition() -> ToolDefinition {
    ToolDefinition::simple("echo", "echo", philo_agent_runtime::EffectClass::ReadOnly)
}

fn runtime(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<FakeTool>,
) -> AgentRuntime {
    AgentRuntime::with_tools(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(),
        tools,
    )
}

fn collect_events(mut handle: philo_agent_runtime::OperationHandle) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = block_on(handle.next_event()) {
        events.push(event);
    }
    events
}

#[test]
fn shared_fixture_executes_one_tool_loop() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::text(&["done"]),
    ]));
    let tools = Arc::new(FakeTool::one(
        echo_definition(),
        FakeToolResult::success("ok"),
    ));
    let sessions = Arc::new(MemorySessionStore::new());
    let handle = block_on(
        runtime(model.clone(), sessions.clone(), tools.clone())
            .prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();

    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "done"
    ));
    let events = collect_events(handle);
    assert_eq!(model.call_count(), 2);
    assert_eq!(tools.invocation_count(), 1);
    assert!(model.calls()[1].tools.is_empty());
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolExecutionStarted { index: 0, .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolExecutionCompleted { index: 0, .. }))
    );
    let view = block_on(sessions.context_view(&philo_session::SessionId::new("session"))).unwrap();
    assert!(matches!(
        view.messages()[1],
        ContextMessage::AssistantToolCalls { .. }
    ));
    assert!(matches!(
        view.messages()[2],
        ContextMessage::ToolResult { .. }
    ));
}

#[test]
fn shared_failing_store_targets_context_read_and_barrier_a() {
    for plan in [FailurePlan::context_read_at(1), FailurePlan::commit_at(1)] {
        let model = Arc::new(FakeModel::succeeds(&["unused"]));
        let tools = Arc::new(FakeTool::new([], []));
        let store = Arc::new(FailingSessionStore::memory(plan));
        let handle = block_on(
            runtime(model.clone(), store, tools)
                .prompt(SessionId::new("session"), UserMessage::new("hi")),
        )
        .unwrap();
        assert!(matches!(
            block_on(handle.wait()),
            OperationOutcome::Failed { .. }
        ));
        assert_eq!(model.call_count(), 0);
    }
}

#[test]
fn shared_failing_store_can_fail_barrier_e_only() {
    let model = Arc::new(FakeModel::start_fails("offline"));
    let tools = Arc::new(FakeTool::new([], []));
    let store = Arc::new(FailingSessionStore::memory(FailurePlan::commit_at(2)));
    let handle = block_on(
        runtime(model.clone(), store.clone(), tools)
            .prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Failed {
            durability: philo_agent_runtime::SettlementDurability::Unconfirmed,
            ..
        }
    ));
    assert_eq!(model.call_count(), 1);
    assert_eq!(store.commit_count(), 2);
}
