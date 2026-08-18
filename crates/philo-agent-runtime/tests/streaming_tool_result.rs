//! L1 display-channel live progress: ordering, last-wins, isolation.

mod support;

use std::sync::Arc;

use philo_agent_runtime::{
    AgentEvent, GenerationConfig, OperationOutcome, RuntimeConfig, SequentialIdSource, SessionId,
    ToolDefinition, ToolResult, UserMessage,
};
use philo_session::{MemorySessionStore, SessionStore, ToolResultOutcome};
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};

fn config(max_parallel: u32) -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "sys".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds: 1,
        max_parallel_tool_calls: max_parallel,
        operation_timeout: None,
        tool_cancel_grace: std::time::Duration::from_millis(300),
        compaction: Default::default(),
    }
}

fn echo() -> ToolDefinition {
    ToolDefinition::simple("echo", "echo", philo_agent_runtime::EffectClass::ReadOnly)
}

async fn collect(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<FakeTool>,
    max_parallel: u32,
) -> (Vec<AgentEvent>, OperationOutcome) {
    let runtime = support::runtime::TestRuntime::with_tools(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(max_parallel),
        tools,
    )
    .await;
    let handle = runtime
        .prompt(SessionId::new("s"), UserMessage::new("hi"))
        .await;
    let outcome = handle.wait().await;
    let mut events = Vec::new();
    while let Some(event) = handle.next_event().await {
        events.push(event);
    }
    (events, outcome)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progress_is_between_started_and_completed_and_not_durable() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::text(&["done"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::one(
        echo(),
        FakeToolResult::streaming_success(["hello", " world"], "model view"),
    ));
    let (events, outcome) = collect(model, sessions.clone(), tools, 1).await;
    assert!(matches!(outcome, OperationOutcome::Succeeded { .. }));

    let started = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ToolExecutionStarted { .. }))
        .expect("started");
    let progress = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ToolExecutionProgress { .. }))
        .expect("progress");
    let completed = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::ToolExecutionCompleted {
                    result: ToolResult::Success { content },
                    ..
                } if content == "model view"
            )
        })
        .expect("completed");
    assert!(started < progress && progress < completed);

    let view = sessions
        .context_view(&philo_session::SessionId::new("s"))
        .await
        .unwrap();
    let durable = view
        .messages()
        .iter()
        .find_map(|message| match message {
            philo_session::ContextMessage::ToolResult { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .expect("durable tool result");
    assert_eq!(
        durable,
        ToolResultOutcome::Success {
            content: "model view".to_owned()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flood_keeps_at_most_one_unconsumed_progress_per_call() {
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
    let (events, _) = collect(model, sessions, tools, 1).await;
    let progress = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::ToolExecutionProgress { .. }))
        .count();
    assert!(
        (1..=2).contains(&progress),
        "unconsumed progress is last-wins, got {progress}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_progress_tails_do_not_mix() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_calls(&[(0, "call-a", "echo", "{}"), (1, "call-b", "echo", "{}")]),
        ModelScript::text(&["done"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::new(
        [echo()],
        [
            FakeToolResult::streaming_success(["AAA"], "a"),
            FakeToolResult::streaming_success(["BBB"], "b"),
        ],
    ));
    let (events, _) = collect(model, sessions, tools, 2).await;
    let tails: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionProgress {
                tool_call_id, tail, ..
            } => Some((tool_call_id.as_str().to_owned(), tail.clone())),
            _ => None,
        })
        .collect();
    assert!(
        !tails.is_empty(),
        "at least one tool progress must be visible: {tails:?}"
    );
    // Driver slots stay per-call. The Service-facing coalescer is one
    // ToolProgress slot per operation, so an unread outlet latest-wins.
    // A surviving event must still carry its own call's tail.
    assert!(
        tails.iter().all(|(id, tail)| {
            !(id.contains("call-a") && tail.contains("BBB"))
                && !(id.contains("call-b") && tail.contains("AAA"))
        }),
        "progress tail must belong to its tool_call_id: {tails:?}"
    );
}
