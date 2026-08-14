mod support;

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use philo_agent_runtime::{
    AgentEvent, AgentFailureKind, AgentRuntime, GenerationConfig, OperationOutcome, RuntimeConfig,
    SequentialIdSource, SessionId, SettlementDurability, ToolDefinition, UserMessage,
};
use philo_session::{MemorySessionStore, SessionStore};
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

fn config(max_tool_rounds: u32) -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "sys".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds,
        operation_timeout: None,
        compaction: Default::default(),
    }
}

fn runtime(
    max_tool_rounds: u32,
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<FakeTool>,
) -> AgentRuntime {
    AgentRuntime::with_tools(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(max_tool_rounds),
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

fn echo_definition() -> ToolDefinition {
    ToolDefinition::simple("echo", "echo", philo_agent_runtime::EffectClass::ReadOnly)
}

fn tool_round(call_id: &str) -> ModelScript {
    ModelScript::tool_call(0, Some(call_id), Some("echo"), &["{}"])
}

/// Commit sequence per round: A=1, then B_k = 2k, C_k = 2k+1, final = 2N+2.
#[test]
fn runtime_m3_001_two_round_loop_follows_tools_allowed() {
    let model = Arc::new(FakeModel::new([
        tool_round("call-1"),
        tool_round("call-2"),
        ModelScript::text(&["final"]),
    ]));
    let tools = Arc::new(FakeTool::new(
        [echo_definition()],
        [
            FakeToolResult::success("one"),
            FakeToolResult::success("two"),
        ],
    ));
    let sessions = Arc::new(MemorySessionStore::new());
    let handle = block_on(
        runtime(2, model.clone(), sessions, tools.clone())
            .prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "final"
    ));

    let calls = model.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].model_call_index, 1);
    assert_eq!(calls[1].model_call_index, 2);
    assert_eq!(calls[2].model_call_index, 3);
    assert!(!calls[0].tools.is_empty(), "round budget open on call 1");
    assert!(!calls[1].tools.is_empty(), "one round left on call 2");
    assert!(calls[2].tools.is_empty(), "budget exhausted on call 3");
    assert_eq!(tools.invocation_count(), 2);

    let events = collect_events(handle);
    let batch_ids = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolBatchRequested { tool_batch_id, .. } => Some(tool_batch_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(batch_ids.len(), 2);
    assert_ne!(batch_ids[0], batch_ids[1]);
}

#[test]
fn runtime_m3_002_zero_rounds_never_exposes_tools() {
    let model = Arc::new(FakeModel::succeeds(&["plain"]));
    let tools = Arc::new(FakeTool::new([echo_definition()], []));
    let handle = block_on(
        runtime(0, model.clone(), Arc::new(MemorySessionStore::new()), tools)
            .prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { .. }
    ));
    assert!(
        model.calls()[0].tools.is_empty(),
        "max_tool_rounds = 0 hides registered tools"
    );
}

#[test]
fn runtime_m3_003_exhausted_call_with_tool_calls_fails_invalid_output() {
    let model = Arc::new(FakeModel::new([tool_round("call-1"), tool_round("call-2")]));
    let tools = Arc::new(FakeTool::new(
        [echo_definition()],
        [FakeToolResult::success("one")],
    ));
    let handle = block_on(
        runtime(
            1,
            model.clone(),
            Arc::new(MemorySessionStore::new()),
            tools.clone(),
        )
        .prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Failed {
            failure,
            durability: SettlementDurability::Confirmed,
        } if failure.kind() == AgentFailureKind::InvalidModelOutput
    ));
    assert_eq!(model.call_count(), 2);
    assert_eq!(tools.invocation_count(), 1);
}

#[test]
fn runtime_m3_004_barrier_b2_failure_keeps_round_two_tools_unexecuted() {
    let model = Arc::new(FakeModel::new([tool_round("call-1"), tool_round("call-2")]));
    let tools = Arc::new(FakeTool::new(
        [echo_definition()],
        [FakeToolResult::success("one")],
    ));
    let store = Arc::new(FailingSessionStore::memory(FailurePlan::commit_at(4)));
    let handle = block_on(
        runtime(2, model.clone(), store, tools.clone())
            .prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Failed {
            durability: SettlementDurability::Confirmed,
            ..
        }
    ));
    assert_eq!(model.call_count(), 2);
    assert_eq!(
        tools.invocation_count(),
        1,
        "round two must not execute after its batch commit fails"
    );
    let batch_events = collect_events(handle)
        .iter()
        .filter(|event| matches!(event, AgentEvent::ToolBatchRequested { .. }))
        .count();
    assert_eq!(batch_events, 1);
}

#[test]
fn runtime_m3_005_barrier_c2_failure_prevents_third_model_call() {
    let model = Arc::new(FakeModel::new([
        tool_round("call-1"),
        tool_round("call-2"),
        ModelScript::text(&["must not run"]),
    ]));
    let tools = Arc::new(FakeTool::new(
        [echo_definition()],
        [
            FakeToolResult::success("one"),
            FakeToolResult::success("two"),
        ],
    ));
    let store = Arc::new(FailingSessionStore::memory(FailurePlan::commit_at(5)));
    let handle = block_on(
        runtime(2, model.clone(), store, tools.clone())
            .prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Failed {
            durability: SettlementDurability::Confirmed,
            ..
        }
    ));
    assert_eq!(model.call_count(), 2, "no model call after C_2 failure");
    assert_eq!(tools.invocation_count(), 2, "no silent tool retry");
    let completed_events = collect_events(handle)
        .iter()
        .filter(|event| matches!(event, AgentEvent::ToolExecutionCompleted { .. }))
        .count();
    assert_eq!(
        completed_events, 1,
        "only round one completion events were published"
    );
}

#[test]
fn runtime_m3_006_round_two_infrastructure_failure_settles_failed() {
    let model = Arc::new(FakeModel::new([
        tool_round("call-1"),
        tool_round("call-2"),
        ModelScript::text(&["must not run"]),
    ]));
    let tools = Arc::new(FakeTool::new(
        [echo_definition()],
        [
            FakeToolResult::success("one"),
            FakeToolResult::infrastructure_error("worker down"),
        ],
    ));
    let handle = block_on(
        runtime(
            2,
            model.clone(),
            Arc::new(MemorySessionStore::new()),
            tools.clone(),
        )
        .prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Failed {
            failure,
            durability: SettlementDurability::Confirmed,
        } if failure.kind() == AgentFailureKind::ToolExecution
    ));
    assert_eq!(model.call_count(), 2);
    assert_eq!(tools.invocation_count(), 2);
}

#[test]
fn runtime_m3_007_persistent_failure_mid_loop_is_unconfirmed() {
    let model = Arc::new(FakeModel::new([tool_round("call-1"), tool_round("call-2")]));
    let tools = Arc::new(FakeTool::new(
        [echo_definition()],
        [FakeToolResult::success("one")],
    ));
    let store = Arc::new(FailingSessionStore::memory(
        FailurePlan::persistent_commit_at(4),
    ));
    let handle = block_on(
        runtime(2, model, store, tools).prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Failed {
            durability: SettlementDurability::Unconfirmed,
            ..
        }
    ));
    assert!(
        !collect_events(handle)
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnFailed { .. })),
        "unconfirmed settlement must not publish durable TurnFailed"
    );
}
