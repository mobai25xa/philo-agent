//! RUNTIME-011: Bounded parallel tool execution: overlap, source-order commit, cancel, and errors.

mod support;

use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use philo_agent_runtime::{
    AgentEvent, AgentFailureKind, AgentRuntime, GenerationConfig, OperationHandle,
    OperationOutcome, RuntimeConfig, SequentialIdSource, SessionId, ToolDefinition, UserMessage,
};
use philo_session::{ContextMessage, MemorySessionStore, SessionStore, ToolResultOutcome};
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};
use support::gate::Gate;

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn poll_once<F: Future + ?Sized>(future: &mut Pin<Box<F>>) -> Poll<F::Output> {
    let mut context = Context::from_waker(Waker::noop());
    future.as_mut().poll(&mut context)
}

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
    }
}

fn runtime(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<FakeTool>,
    max_parallel_tool_calls: u32,
) -> AgentRuntime {
    AgentRuntime::with_tools(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(max_parallel_tool_calls),
        tools,
    )
}

fn echo() -> ToolDefinition {
    ToolDefinition::simple("echo", "echo", philo_agent_runtime::EffectClass::ReadOnly)
}

fn sid() -> SessionId {
    SessionId::new("s")
}

fn drive_until(
    wait: &mut Pin<Box<impl Future<Output = OperationOutcome>>>,
    ready: impl Fn() -> bool,
) {
    for _ in 0..10_000 {
        if ready() {
            return;
        }
        assert!(poll_once(wait).is_pending(), "operation settled too early");
    }
    panic!("timed out waiting for the tool batch to reach the expected state");
}

fn result_outcomes(sessions: &MemorySessionStore) -> Vec<(String, ToolResultOutcome)> {
    let view = block_on(sessions.context_view(&philo_session::SessionId::new("s"))).unwrap();
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

fn collect_events(mut handle: OperationHandle) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = block_on(handle.next_event()) {
        events.push(event);
    }
    events
}

#[test]
fn limit_one_keeps_two_calls_strictly_serial() {
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
    let agent = runtime(model.clone(), sessions.clone(), tools.clone(), 1);
    let handle = block_on(agent.prompt(sid(), UserMessage::new("hi"))).unwrap();
    let mut wait = Box::pin(handle.wait());

    drive_until(&mut wait, || tools.invocation_count() == 1);
    for _ in 0..32 {
        assert!(poll_once(&mut wait).is_pending());
        assert_eq!(
            tools.invocation_count(),
            1,
            "the second call stays unstarted"
        );
    }

    first.release();
    drive_until(&mut wait, || tools.invocation_count() == 2);
    second.release();
    assert!(matches!(block_on(wait), OperationOutcome::Succeeded { .. }));
    assert_eq!(
        result_outcomes(&sessions)
            .into_iter()
            .map(|(id, outcome)| (id, matches!(outcome, ToolResultOutcome::Success { .. })))
            .collect::<Vec<_>>(),
        [("call-1".to_owned(), true), ("call-2".to_owned(), true)]
    );
    assert_eq!(model.calls()[0].max_parallel_tool_calls, 1);
}

#[test]
fn limit_one_cancel_is_still_a_real_prefix() {
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
    let agent = runtime(model, sessions.clone(), tools.clone(), 1);
    let handle = block_on(agent.prompt(sid(), UserMessage::new("hi"))).unwrap();
    let mut wait = Box::pin(handle.wait());
    drive_until(&mut wait, || tools.invocation_count() == 1);
    handle.cancel();
    tool_gate.release();
    assert!(matches!(block_on(wait), OperationOutcome::Cancelled));
    assert_eq!(tools.invocation_count(), 1);
    assert_eq!(
        result_outcomes(&sessions)
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

#[test]
fn limit_two_overlaps_invokes_and_commits_in_source_order() {
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
    let agent = runtime(model.clone(), sessions.clone(), tools.clone(), 2);
    let handle = block_on(agent.prompt(sid(), UserMessage::new("hi"))).unwrap();
    let mut wait = Box::pin(handle.wait());

    drive_until(&mut wait, || tools.invocation_count() == 2);
    assert_eq!(
        tools.invocation_count(),
        2,
        "both invokes are in flight before either gate opens"
    );
    second.release();
    first.release();
    assert!(matches!(block_on(wait), OperationOutcome::Succeeded { .. }));

    let outcomes = result_outcomes(&sessions);
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

#[test]
fn cancel_awaits_in_flight_and_marks_unstarted() {
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
    let agent = runtime(model, sessions.clone(), tools.clone(), 2);
    let handle = block_on(agent.prompt(sid(), UserMessage::new("hi"))).unwrap();
    let mut wait = Box::pin(handle.wait());
    drive_until(&mut wait, || tools.invocation_count() == 2);
    handle.cancel();
    first.release();
    second.release();
    assert!(matches!(block_on(wait), OperationOutcome::Cancelled));
    assert_eq!(tools.invocation_count(), 2, "the third call never starts");

    let outcomes = result_outcomes(&sessions);
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

    let events = collect_events(handle);
    let completed: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionCompleted { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(completed, ["call-1", "call-2"]);
}

#[test]
fn tool_port_error_awaits_other_in_flight_and_fails_the_turn() {
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
    let agent = runtime(model, sessions, tools.clone(), 2);
    let handle = block_on(agent.prompt(sid(), UserMessage::new("hi"))).unwrap();
    let mut wait = Box::pin(handle.wait());
    drive_until(&mut wait, || tools.invocation_count() == 2);
    assert!(
        poll_once(&mut wait).is_pending(),
        "the turn must await the remaining in-flight invoke"
    );
    second.release();
    match block_on(wait) {
        OperationOutcome::Failed { failure, .. } => {
            assert_eq!(failure.kind(), AgentFailureKind::ToolExecution);
            assert!(failure.message().contains("worker down"));
        }
        other => panic!("expected a tool-port failure, got {other:?}"),
    }
    assert_eq!(tools.invocation_count(), 2);
}
