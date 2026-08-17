//! RUNTIME-012: ToolPort interrupt protocol — eager signal, completion
//! wins, Stopped/drop → Interrupted, grace drop.

mod support;

use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use philo_agent_runtime::{
    AgentEvent, AgentRuntime, DEFAULT_TOOL_CANCEL_GRACE, GenerationConfig, OperationHandle,
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

fn runtime(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<FakeTool>,
    max_parallel: u32,
    grace: Duration,
) -> AgentRuntime {
    AgentRuntime::with_tools(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(max_parallel, grace),
        tools,
    )
}

fn echo() -> ToolDefinition {
    ToolDefinition::simple("echo", "echo", philo_agent_runtime::EffectClass::ReadOnly)
}

fn sid() -> SessionId {
    SessionId::new("s")
}

fn drive_until<F: Future + ?Sized>(future: &mut Pin<Box<F>>, ready: impl Fn() -> bool) {
    for _ in 0..10_000 {
        if ready() {
            return;
        }
        let _ = poll_once(future);
        std::thread::yield_now();
    }
    panic!("timed out waiting for tool progress");
}

fn collect_events(mut handle: OperationHandle) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = block_on(handle.next_event()) {
        events.push(event);
    }
    events
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

#[test]
fn stops_on_cancel_records_interrupted_without_completed_event() {
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
    );
    let handle = block_on(agent.prompt(sid(), UserMessage::new("hi"))).unwrap();
    let mut wait = Box::pin(handle.wait());
    drive_until(&mut wait, || tools.invocation_count() == 1);
    handle.cancel();
    assert!(matches!(block_on(wait), OperationOutcome::Cancelled));
    assert!(matches!(
        result_outcomes(&sessions).as_slice(),
        [(id, ToolResultOutcome::Interrupted)] if id == "call-1"
    ));
    let completed = collect_events(handle)
        .iter()
        .any(|event| matches!(event, AgentEvent::ToolExecutionCompleted { .. }));
    assert!(!completed, "Stopped does not publish Completed");
}

#[test]
fn completion_wins_when_gated_tool_finishes_after_cancel() {
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
    );
    let handle = block_on(agent.prompt(sid(), UserMessage::new("hi"))).unwrap();
    let mut wait = Box::pin(handle.wait());
    drive_until(&mut wait, || tools.invocation_count() == 1);
    handle.cancel();
    gate.release();
    assert!(matches!(block_on(wait), OperationOutcome::Cancelled));
    assert!(matches!(
        result_outcomes(&sessions).as_slice(),
        [(id, ToolResultOutcome::Success { content })] if id == "call-1" && content == "done"
    ));
}

#[test]
fn grace_zero_drops_ignore_cancel_as_interrupted() {
    let model = Arc::new(FakeModel::new([ModelScript::tool_calls(&[(
        0, "call-1", "echo", "{}",
    )])]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::new([echo()], [FakeToolResult::ignores_cancel()]));
    let agent = runtime(model, sessions.clone(), tools.clone(), 1, Duration::ZERO);
    let handle = block_on(agent.prompt(sid(), UserMessage::new("hi"))).unwrap();
    let mut wait = Box::pin(handle.wait());
    drive_until(&mut wait, || tools.invocation_count() == 1);
    handle.cancel();
    assert!(matches!(block_on(wait), OperationOutcome::Cancelled));
    assert!(matches!(
        result_outcomes(&sessions).as_slice(),
        [(id, ToolResultOutcome::Interrupted)] if id == "call-1"
    ));
}

#[test]
fn parallel_stopped_and_unstarted_use_per_slot_marks() {
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
    );
    let handle = block_on(agent.prompt(sid(), UserMessage::new("hi"))).unwrap();
    let mut wait = Box::pin(handle.wait());
    drive_until(&mut wait, || tools.invocation_count() == 2);
    handle.cancel();
    assert!(matches!(block_on(wait), OperationOutcome::Cancelled));
    assert_eq!(tools.invocation_count(), 2);
    let outcomes = result_outcomes(&sessions);
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

#[test]
fn port_error_after_cancel_stays_cancelled() {
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
    );
    let handle = block_on(agent.prompt(sid(), UserMessage::new("hi"))).unwrap();
    let mut wait = Box::pin(handle.wait());
    drive_until(&mut wait, || tools.invocation_count() == 1);
    handle.cancel();
    gate.release();
    assert!(matches!(block_on(wait), OperationOutcome::Cancelled));
    assert!(matches!(
        result_outcomes(&sessions).as_slice(),
        [(id, ToolResultOutcome::Interrupted)] if id == "call-1"
    ));
}
