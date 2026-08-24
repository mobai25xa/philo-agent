//! RUNTIME-011: Bounded parallel tool execution: overlap, source-order commit, cancel, and errors.

mod support;

use std::sync::Arc;

use philo_agent_runtime::{
    AgentEvent, DurableFailureKind, GenerationConfig, OperationOutcome, RuntimeConfig,
    SequentialIdSource, SessionId, ToolDefinition, UserMessage,
};
use philo_session::{ContextMessage, MemorySessionStore, SessionStore, ToolResultOutcome};
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};
use support::gate::Gate;

fn config(max_parallel_tool_calls: u32) -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "sys".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds: 1,
        max_parallel_tool_calls,
        operation_timeout: None,
        tool_cancel_grace: std::time::Duration::from_millis(300),
        compaction: Default::default(),
        recovery: Default::default(),
    }
}

async fn runtime(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<FakeTool>,
    max_parallel_tool_calls: u32,
) -> support::runtime::TestRuntime {
    support::runtime::TestRuntime::with_tools(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(max_parallel_tool_calls),
        tools,
    )
    .await
}

fn echo() -> ToolDefinition {
    ToolDefinition::simple("echo", "echo", philo_agent_runtime::EffectClass::ReadOnly)
}

fn sid() -> SessionId {
    SessionId::new("s")
}

async fn result_outcomes(sessions: &MemorySessionStore) -> Vec<(String, ToolResultOutcome)> {
    let view = sessions
        .context_view(&philo_session::SessionId::new("s"))
        .await
        .unwrap();
    view.messages()
        .iter()
        .filter_map(|message| match message {
            ContextMessage::ToolResult {
                tool_call_id,
                outcome,
            } => Some((tool_call_id.as_str().to_owned(), outcome.clone())),
            _ => None,
        })
        .collect()
}

async fn collect_events(handle: &support::runtime::TestOp) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = handle.next_event().await {
        events.push(event);
    }
    events
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limit_one_keeps_two_calls_strictly_serial() {
    let first = Gate::new();
    let second = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_calls(&[(0, "call-1", "echo", "{}"), (1, "call-2", "echo", "{}")]),
        ModelScript::text(&["done"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::new(
        [echo()],
        [
            FakeToolResult::gated_success(&first, "one"),
            FakeToolResult::gated_success(&second, "two"),
        ],
    ));
    let agent = runtime(model.clone(), sessions.clone(), tools.clone(), 1).await;
    let handle = agent.prompt(sid(), UserMessage::new("hi")).await;

    support::runtime::wait_until(|| tools.invocation_count() == 1).await;
    for _ in 0..32 {
        tokio::task::yield_now().await;
        assert_eq!(
            tools.invocation_count(),
            1,
            "the second call stays unstarted"
        );
    }

    first.release();
    support::runtime::wait_until(|| tools.invocation_count() == 2).await;
    second.release();
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { .. }
    ));
    assert_eq!(
        result_outcomes(&sessions)
            .await
            .into_iter()
            .map(|(id, outcome)| (id, matches!(outcome, ToolResultOutcome::Success { .. })))
            .collect::<Vec<_>>(),
        [("call-1".to_owned(), true), ("call-2".to_owned(), true)]
    );
    assert_eq!(model.calls()[0].max_parallel_tool_calls, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limit_one_cancel_is_still_a_real_prefix() {
    let tool_gate = Gate::new();
    let model = Arc::new(FakeModel::new([ModelScript::tool_calls(&[
        (0, "call-1", "echo", "{}"),
        (1, "call-2", "echo", "{}"),
    ])]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::new(
        [echo()],
        [
            FakeToolResult::gated_success(&tool_gate, "one"),
            FakeToolResult::success("never"),
        ],
    ));
    let agent = runtime(model, sessions.clone(), tools.clone(), 1).await;
    let handle = agent.prompt(sid(), UserMessage::new("hi")).await;
    support::runtime::wait_until(|| tools.invocation_count() == 1).await;
    handle.cancel().await;
    tool_gate.release();
    assert!(matches!(handle.wait().await, OperationOutcome::Cancelled));
    assert_eq!(tools.invocation_count(), 1);
    assert_eq!(
        result_outcomes(&sessions)
            .await
            .into_iter()
            .map(|(id, outcome)| {
                (
                    id,
                    matches!(outcome, ToolResultOutcome::Success { .. }),
                    matches!(outcome, ToolResultOutcome::Cancelled),
                )
            })
            .collect::<Vec<_>>(),
        [
            ("call-1".to_owned(), true, false),
            ("call-2".to_owned(), false, true)
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limit_two_overlaps_invokes_and_commits_in_source_order() {
    let first = Gate::new();
    let second = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_calls(&[(0, "call-1", "echo", "{}"), (1, "call-2", "echo", "{}")]),
        ModelScript::text(&["done"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::new(
        [echo()],
        [
            FakeToolResult::gated_success(&first, "one"),
            FakeToolResult::gated_success(&second, "two"),
        ],
    ));
    let agent = runtime(model.clone(), sessions.clone(), tools.clone(), 2).await;
    let handle = agent.prompt(sid(), UserMessage::new("hi")).await;

    support::runtime::wait_until(|| tools.invocation_count() == 2).await;
    assert_eq!(
        tools.invocation_count(),
        2,
        "both invokes are in flight before either gate opens"
    );
    second.release();
    first.release();
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { .. }
    ));

    let outcomes = result_outcomes(&sessions).await;
    assert_eq!(
        outcomes
            .iter()
            .map(|(id, outcome)| (id.as_str(), outcome))
            .collect::<Vec<_>>(),
        [
            (
                "call-1",
                &ToolResultOutcome::Success {
                    content: "one".to_owned()
                }
            ),
            (
                "call-2",
                &ToolResultOutcome::Success {
                    content: "two".to_owned()
                }
            ),
        ]
    );
    assert_eq!(model.calls()[0].max_parallel_tool_calls, 2);
    assert_eq!(
        model.call_count(),
        2,
        "C_k happens once before the next model call"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_awaits_in_flight_and_marks_unstarted() {
    let first = Gate::new();
    let second = Gate::new();
    let model = Arc::new(FakeModel::new([ModelScript::tool_calls(&[
        (0, "call-1", "echo", "{}"),
        (1, "call-2", "echo", "{}"),
        (2, "call-3", "echo", "{}"),
    ])]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::new(
        [echo()],
        [
            FakeToolResult::gated_success(&first, "one"),
            FakeToolResult::gated_success(&second, "two"),
            FakeToolResult::success("never"),
        ],
    ));
    let agent = runtime(model, sessions.clone(), tools.clone(), 2).await;
    let handle = agent.prompt(sid(), UserMessage::new("hi")).await;
    support::runtime::wait_until(|| tools.invocation_count() == 2).await;
    handle.cancel().await;
    first.release();
    second.release();
    assert!(matches!(handle.wait().await, OperationOutcome::Cancelled));
    assert_eq!(tools.invocation_count(), 2, "the third call never starts");

    let outcomes = result_outcomes(&sessions).await;
    assert_eq!(outcomes.len(), 3);
    assert!(matches!(
        &outcomes[0],
        (id, ToolResultOutcome::Success { content }) if id == "call-1" && content == "one"
    ));
    assert!(matches!(
        &outcomes[1],
        (id, ToolResultOutcome::Success { content }) if id == "call-2" && content == "two"
    ));
    assert!(matches!(
        &outcomes[2],
        (id, ToolResultOutcome::Cancelled) if id == "call-3"
    ));

    let events = collect_events(&handle).await;
    let completed: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionCompleted { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(completed, ["call-1", "call-2"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_port_error_awaits_other_in_flight_and_fails_the_turn() {
    let second = Gate::new();
    let model = Arc::new(FakeModel::new([ModelScript::tool_calls(&[
        (0, "call-1", "echo", "{}"),
        (1, "call-2", "echo", "{}"),
    ])]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::new(
        [echo()],
        [
            FakeToolResult::infrastructure_error("worker down"),
            FakeToolResult::gated_success(&second, "late"),
        ],
    ));
    let agent = runtime(model, sessions, tools.clone(), 2).await;
    let handle = agent.prompt(sid(), UserMessage::new("hi")).await;
    support::runtime::wait_until(|| tools.invocation_count() == 2).await;
    assert_eq!(
        tools.invocation_count(),
        2,
        "the turn must await the remaining in-flight invoke"
    );
    second.release();
    match handle.wait().await {
        OperationOutcome::Failed { failure, .. } => {
            assert_eq!(failure.durable_kind(), DurableFailureKind::ToolExecution);
            assert!(failure.diagnostic().contains("worker down"));
        }
        other => panic!("expected a tool-port failure, got {other:?}"),
    }
    assert_eq!(tools.invocation_count(), 2);
}
