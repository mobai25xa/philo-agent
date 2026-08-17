//! RUNTIME-012: ToolPort interrupt protocol — eager signal, completion
//! wins, Stopped/drop → Interrupted, grace drop.

mod support;

use std::sync::Arc;
use std::time::Duration;

use philo_agent_runtime::{
    AgentEvent, DEFAULT_TOOL_CANCEL_GRACE, GenerationConfig, OperationOutcome, RuntimeConfig,
    SequentialIdSource, SessionId, ToolDefinition, UserMessage,
};
use philo_session::{ContextMessage, MemorySessionStore, SessionStore, ToolResultOutcome};
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};
use support::gate::Gate;

fn config(max_parallel: u32, grace: Duration) -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "sys".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds: 1,
        max_parallel_tool_calls: max_parallel,
        operation_timeout: None,
        tool_cancel_grace: grace,
        compaction: Default::default(),
    }
}

async fn runtime(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<FakeTool>,
    max_parallel: u32,
    grace: Duration,
) -> support::runtime::TestRuntime {
    support::runtime::TestRuntime::with_tools(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(max_parallel, grace),
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

async fn collect_events(handle: &support::runtime::TestOp) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = handle.next_event().await {
        events.push(event);
    }
    events
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stops_on_cancel_records_interrupted_without_completed_event() {
    let model = Arc::new(FakeModel::new([ModelScript::tool_calls(&[(
        0, "call-1", "echo", "{}",
    )])]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::new([echo()], [FakeToolResult::stops_on_cancel()]));
    let agent = runtime(
        model,
        sessions.clone(),
        tools.clone(),
        1,
        DEFAULT_TOOL_CANCEL_GRACE,
    )
    .await;
    let handle = agent.prompt(sid(), UserMessage::new("hi")).await;
    support::runtime::wait_until(|| tools.invocation_count() == 1).await;
    handle.cancel().await;
    assert!(matches!(handle.wait().await, OperationOutcome::Cancelled));
    assert!(matches!(
        result_outcomes(&sessions).await.as_slice(),
        [(id, ToolResultOutcome::Interrupted)] if id == "call-1"
    ));
    let completed = collect_events(&handle)
        .await
        .iter()
        .any(|event| matches!(event, AgentEvent::ToolExecutionCompleted { .. }));
    assert!(!completed, "Stopped does not publish Completed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_wins_when_gated_tool_finishes_after_cancel() {
    let gate = Gate::new();
    let model = Arc::new(FakeModel::new([ModelScript::tool_calls(&[(
        0, "call-1", "echo", "{}",
    )])]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::new(
        [echo()],
        [FakeToolResult::gated_success(&gate, "done")],
    ));
    let agent = runtime(
        model,
        sessions.clone(),
        tools.clone(),
        1,
        DEFAULT_TOOL_CANCEL_GRACE,
    )
    .await;
    let handle = agent.prompt(sid(), UserMessage::new("hi")).await;
    support::runtime::wait_until(|| tools.invocation_count() == 1).await;
    handle.cancel().await;
    gate.release();
    assert!(matches!(handle.wait().await, OperationOutcome::Cancelled));
    assert!(matches!(
        result_outcomes(&sessions).await.as_slice(),
        [(id, ToolResultOutcome::Success { content })] if id == "call-1" && content == "done"
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grace_zero_drops_ignore_cancel_as_interrupted() {
    let model = Arc::new(FakeModel::new([ModelScript::tool_calls(&[(
        0, "call-1", "echo", "{}",
    )])]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::new([echo()], [FakeToolResult::ignores_cancel()]));
    let agent = runtime(model, sessions.clone(), tools.clone(), 1, Duration::ZERO).await;
    let handle = agent.prompt(sid(), UserMessage::new("hi")).await;
    support::runtime::wait_until(|| tools.invocation_count() == 1).await;
    handle.cancel().await;
    assert!(matches!(handle.wait().await, OperationOutcome::Cancelled));
    assert!(matches!(
        result_outcomes(&sessions).await.as_slice(),
        [(id, ToolResultOutcome::Interrupted)] if id == "call-1"
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_stopped_and_unstarted_use_per_slot_marks() {
    let model = Arc::new(FakeModel::new([ModelScript::tool_calls(&[
        (0, "call-1", "echo", "{}"),
        (1, "call-2", "echo", "{}"),
        (2, "call-3", "echo", "{}"),
    ])]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::new(
        [echo()],
        [
            FakeToolResult::stops_on_cancel(),
            FakeToolResult::stops_on_cancel(),
            FakeToolResult::success("never"),
        ],
    ));
    let agent = runtime(
        model,
        sessions.clone(),
        tools.clone(),
        2,
        DEFAULT_TOOL_CANCEL_GRACE,
    )
    .await;
    let handle = agent.prompt(sid(), UserMessage::new("hi")).await;
    support::runtime::wait_until(|| tools.invocation_count() == 2).await;
    handle.cancel().await;
    assert!(matches!(handle.wait().await, OperationOutcome::Cancelled));
    assert_eq!(tools.invocation_count(), 2);
    let outcomes = result_outcomes(&sessions).await;
    assert_eq!(outcomes.len(), 3);
    assert!(matches!(
        &outcomes[0],
        (id, ToolResultOutcome::Interrupted) if id == "call-1"
    ));
    assert!(matches!(
        &outcomes[1],
        (id, ToolResultOutcome::Interrupted) if id == "call-2"
    ));
    assert!(matches!(
        &outcomes[2],
        (id, ToolResultOutcome::Cancelled) if id == "call-3"
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn port_error_after_cancel_stays_cancelled() {
    let gate = Gate::new();
    let model = Arc::new(FakeModel::new([ModelScript::tool_calls(&[(
        0, "call-1", "echo", "{}",
    )])]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::new(
        [echo()],
        [FakeToolResult::gated_infrastructure_error(&gate, "late")],
    ));
    let agent = runtime(
        model,
        sessions.clone(),
        tools.clone(),
        1,
        DEFAULT_TOOL_CANCEL_GRACE,
    )
    .await;
    let handle = agent.prompt(sid(), UserMessage::new("hi")).await;
    support::runtime::wait_until(|| tools.invocation_count() == 1).await;
    handle.cancel().await;
    gate.release();
    assert!(matches!(handle.wait().await, OperationOutcome::Cancelled));
    assert!(matches!(
        result_outcomes(&sessions).await.as_slice(),
        [(id, ToolResultOutcome::Interrupted)] if id == "call-1"
    ));
}
