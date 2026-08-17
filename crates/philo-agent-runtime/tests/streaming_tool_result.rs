//! L1 display-channel live progress: ordering, last-wins, isolation.

mod support;

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use philo_agent_runtime::{
    AgentEvent, AgentRuntime, GenerationConfig, OperationOutcome, RuntimeConfig,
    SequentialIdSource, SessionId, ToolDefinition, ToolResult, UserMessage,
};
use philo_session::{MemorySessionStore, SessionStore, ToolResultOutcome};
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn config(max_parallel: u32) -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "sys".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds: 1,
        max_parallel_tool_calls: max_parallel,
        operation_timeout: None,
        compaction: Default::default(),
    }
}

fn echo() -> ToolDefinition {
    ToolDefinition::simple("echo", "echo", philo_agent_runtime::EffectClass::ReadOnly)
}

fn collect(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<FakeTool>,
    max_parallel: u32,
) -> (Vec<AgentEvent>, OperationOutcome) {
    let runtime = AgentRuntime::with_tools(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(max_parallel),
        tools,
    );
    let mut handle = block_on(runtime.prompt(SessionId::new("s"), UserMessage::new("hi"))).unwrap();
    let outcome = block_on(handle.wait());
    let mut events = Vec::new();
    while let Some(event) = block_on(handle.next_event()) {
        events.push(event);
    }
    (events, outcome)
}

#[test]
fn progress_is_between_started_and_completed_and_not_durable() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::text(&["done"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::one(
        echo(),
        FakeToolResult::streaming_success(["hello", " world"], "model view"),
    ));
    let (events, outcome) = collect(model, sessions.clone(), tools, 1);
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

    let view = block_on(sessions.context_view(&philo_session::SessionId::new("s"))).unwrap();
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

#[test]
fn flood_keeps_at_most_one_unconsumed_progress_per_call() {
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
    let (events, _) = collect(model, sessions, tools, 1);
    let progress = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::ToolExecutionProgress { .. }))
        .count();
    assert_eq!(progress, 1, "unconsumed progress is last-wins");
}

#[test]
fn parallel_progress_tails_do_not_mix() {
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
    let (events, _) = collect(model, sessions, tools, 2);
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
        tails
            .iter()
            .any(|(id, tail)| id.contains("call-a") && tail.contains("AAA")),
        "{tails:?}"
    );
    assert!(
        tails
            .iter()
            .any(|(id, tail)| id.contains("call-b") && tail.contains("BBB")),
        "{tails:?}"
    );
    assert!(
        tails
            .iter()
            .all(|(id, tail)| !(id.contains("call-a") && tail.contains("BBB"))),
        "{tails:?}"
    );
}
