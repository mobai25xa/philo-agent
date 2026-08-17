//! RUNTIME-003: 多轮、耗尽、回放

mod support;

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use philo_agent_runtime::{
    AgentEvent, AgentFailureKind, AgentRuntime, GenerationConfig, ModelMessage, OperationOutcome,
    RuntimeConfig, SequentialIdSource, SessionId, SettlementDurability, ToolDefinition,
    UserMessage,
};
use philo_session::{ContextMessage, MemorySessionStore, SessionStore, ToolResultOutcome};
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
        max_parallel_tool_calls: 1,
        operation_timeout: None,
        tool_cancel_grace: std::time::Duration::from_millis(300),
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

fn context_kinds(view: &philo_session::SessionContextView) -> Vec<&'static str> {
    view.messages()
        .iter()
        .map(|message| match message {
            ContextMessage::Summary { .. } => "summary",
            ContextMessage::User { .. } => "user",
            ContextMessage::AssistantToolCalls { .. } => "calls",
            ContextMessage::ToolResult { .. } => "result",
            ContextMessage::Assistant { .. } => "assistant",
        })
        .collect()
}

/// Commit sequence per round: A=1, then B_k = 2k, C_k = 2k+1, final = 2N+2.
#[test]
fn two_round_loop_follows_tools_allowed() {
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
fn zero_rounds_never_exposes_tools() {
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
fn exhausted_call_with_tool_calls_fails_invalid_output() {
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
fn barrier_b2_failure_keeps_round_two_tools_unexecuted() {
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
fn barrier_c2_failure_prevents_third_model_call() {
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
fn round_two_infrastructure_failure_settles_failed() {
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
fn persistent_failure_mid_loop_is_unconfirmed() {
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

#[test]
fn two_round_loop_persists_all_facts_in_order() {
    let model = Arc::new(FakeModel::new([
        tool_round("call-1"),
        ModelScript::tool_calls(&[(0, "call-2", "echo", "{}"), (1, "call-3", "echo", "{}")]),
        ModelScript::text(&["final answer"]),
    ]));
    let tools = Arc::new(FakeTool::new(
        [echo_definition()],
        [
            FakeToolResult::success("one"),
            FakeToolResult::success("two"),
            FakeToolResult::success("three"),
        ],
    ));
    let sessions = Arc::new(MemorySessionStore::new());
    let handle = block_on(
        runtime(2, model.clone(), sessions.clone(), tools.clone())
            .prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "final answer"
    ));
    assert_eq!(model.call_count(), 3);
    assert_eq!(tools.invocation_count(), 3);
    assert_eq!(
        tools
            .invocations()
            .iter()
            .map(|invocation| invocation.call_id().to_owned())
            .collect::<Vec<_>>(),
        ["call-1", "call-2", "call-3"]
    );

    let view = block_on(sessions.context_view(&philo_session::SessionId::new("session"))).unwrap();
    assert_eq!(
        context_kinds(&view),
        vec![
            "user",
            "calls",
            "result",
            "calls",
            "result",
            "result",
            "assistant"
        ]
    );
    assert_eq!(view.revision(), philo_session::SessionRevision::new(6));

    let events = collect_events(handle);
    let round_markers = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolBatchRequested { tool_batch_id, .. } => {
                Some(("batch", tool_batch_id.as_str().to_owned()))
            }
            AgentEvent::ToolExecutionCompleted { tool_call_id, .. } => {
                Some(("done", tool_call_id.as_str().to_owned()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(round_markers.len(), 5, "two batches and three completions");
    assert_eq!(round_markers[0].0, "batch");
    assert_eq!(round_markers[1], ("done", "call-1".to_owned()));
    assert_eq!(round_markers[2].0, "batch");
    assert_eq!(round_markers[3].1, "call-2");
    assert_eq!(round_markers[4].1, "call-3");
}

#[test]
fn exhausting_rounds_forces_tool_free_final_call() {
    let model = Arc::new(FakeModel::new([
        tool_round("call-1"),
        tool_round("call-2"),
        ModelScript::text(&["forced final"]),
    ]));
    let tools = Arc::new(FakeTool::new(
        [echo_definition()],
        [
            FakeToolResult::success("one"),
            FakeToolResult::success("two"),
        ],
    ));
    let handle = block_on(
        runtime(2, model.clone(), Arc::new(MemorySessionStore::new()), tools)
            .prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "forced final"
    ));
    let calls = model.calls();
    assert!(!calls[0].tools.is_empty());
    assert!(!calls[1].tools.is_empty());
    assert!(
        calls[2].tools.is_empty(),
        "call after round exhaustion must not expose tools"
    );
}

#[test]
fn tool_calls_after_exhaustion_fail_invalid_output() {
    let model = Arc::new(FakeModel::new([tool_round("call-1"), tool_round("call-2")]));
    let tools = Arc::new(FakeTool::new(
        [echo_definition()],
        [FakeToolResult::success("one")],
    ));
    let sessions = Arc::new(MemorySessionStore::new());
    let handle = block_on(
        runtime(1, model.clone(), sessions.clone(), tools.clone())
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
    let view = block_on(sessions.context_view(&philo_session::SessionId::new("session"))).unwrap();
    assert_eq!(
        context_kinds(&view),
        vec!["user", "calls", "result"],
        "durable facts stop at the completed round"
    );
}

#[test]
fn mid_round_tool_error_keeps_later_rounds_available() {
    let model = Arc::new(FakeModel::new([
        tool_round("call-1"),
        tool_round("call-2"),
        ModelScript::text(&["recovered"]),
    ]));
    let tools = Arc::new(FakeTool::new(
        [echo_definition()],
        [
            FakeToolResult::business_error("domain_error", "bad input"),
            FakeToolResult::success("two"),
        ],
    ));
    let sessions = Arc::new(MemorySessionStore::new());
    let handle = block_on(
        runtime(2, model.clone(), sessions.clone(), tools.clone())
            .prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "recovered"
    ));
    assert!(
        !model.calls()[1].tools.is_empty(),
        "an error result does not disable tools early"
    );
    assert_eq!(tools.invocation_count(), 2);
    let view = block_on(sessions.context_view(&philo_session::SessionId::new("session"))).unwrap();
    let outcomes = view
        .messages()
        .iter()
        .filter_map(|message| match message {
            ContextMessage::ToolResult { outcome, .. } => Some(outcome),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(matches!(outcomes[0], ToolResultOutcome::Error { .. }));
    assert!(matches!(outcomes[1], ToolResultOutcome::Success { .. }));
}

#[test]
fn round_two_batch_commit_failure_executes_no_tool() {
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
    assert_eq!(
        tools.invocation_count(),
        1,
        "barrier B_2 failure keeps round-two tool calls at zero"
    );
    assert!(
        !collect_events(handle)
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolExecutionStarted { index: 1, .. })),
    );
}

#[test]
fn round_two_results_commit_failure_stops_next_model_call() {
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
    assert_eq!(model.call_count(), 2, "no third model call after C_2 fails");
    assert_eq!(
        tools.invocation_count(),
        2,
        "executed tools are not retried"
    );
}

#[test]
fn next_prompt_replays_all_rounds_in_source_order() {
    let model = Arc::new(FakeModel::new([
        tool_round("call-1"),
        tool_round("call-2"),
        ModelScript::text(&["multi-round answer"]),
        ModelScript::text(&["second turn answer"]),
    ]));
    let tools = Arc::new(FakeTool::new(
        [echo_definition()],
        [
            FakeToolResult::success("one"),
            FakeToolResult::success("two"),
        ],
    ));
    let sessions = Arc::new(MemorySessionStore::new());
    let agent = runtime(2, model.clone(), sessions, tools);
    assert!(matches!(
        block_on(agent.prompt(SessionId::new("session"), UserMessage::new("first")))
            .map(|handle| block_on(handle.wait())),
        Ok(OperationOutcome::Succeeded { .. })
    ));
    assert!(matches!(
        block_on(agent.prompt(SessionId::new("session"), UserMessage::new("second")))
            .map(|handle| block_on(handle.wait())),
        Ok(OperationOutcome::Succeeded { .. })
    ));

    let second_turn_messages = &model.calls()[3].messages;
    let kinds = second_turn_messages
        .iter()
        .map(|message| match message {
            ModelMessage::System { .. } => "system",
            ModelMessage::Summary { .. } => "summary",
            ModelMessage::User { .. } => "user",
            ModelMessage::AssistantToolCalls { .. } => "calls",
            ModelMessage::ToolResult { .. } => "result",
            ModelMessage::Assistant { .. } => "assistant",
        })
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            "system",
            "user",
            "calls",
            "result",
            "calls",
            "result",
            "assistant",
            "user"
        ],
        "both rounds replay in durable source order for the next turn"
    );
}

#[test]
fn direct_answer_and_single_round_still_pass() {
    // Direct answer under the default-style budget: no tools requested.
    let direct_model = Arc::new(FakeModel::succeeds(&["hello"]));
    let direct_tools = Arc::new(FakeTool::new([], []));
    let handle = block_on(
        runtime(
            8,
            direct_model.clone(),
            Arc::new(MemorySessionStore::new()),
            direct_tools.clone(),
        )
        .prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "hello"
    ));
    assert_eq!(direct_model.call_count(), 1);
    assert_eq!(direct_tools.invocation_count(), 0);

    // One-round loop under explicit max_tool_rounds = 1 keeps the M2 trace.
    let loop_model = Arc::new(FakeModel::new([
        tool_round("call-1"),
        ModelScript::text(&["done"]),
    ]));
    let loop_tools = Arc::new(FakeTool::new(
        [echo_definition()],
        [FakeToolResult::success("ok")],
    ));
    let sessions = Arc::new(MemorySessionStore::new());
    let handle = block_on(
        runtime(1, loop_model.clone(), sessions.clone(), loop_tools.clone())
            .prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "done"
    ));
    assert_eq!(loop_model.call_count(), 2);
    assert!(loop_model.calls()[1].tools.is_empty());
    assert_eq!(loop_tools.invocation_count(), 1);
    let view = block_on(sessions.context_view(&philo_session::SessionId::new("session"))).unwrap();
    assert_eq!(
        context_kinds(&view),
        vec!["user", "calls", "result", "assistant"]
    );
}
