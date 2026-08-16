//! RUNTIME-008: dual-channel absorption, real-time event consumption,
//! mid-run cancellation reachability, and tool_choice freezing.

mod support;

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use philo_agent_runtime::{
    AgentEvent, AgentRuntime, GenerationConfig, OperationOutcome, RuntimeConfig,
    SequentialIdSource, SessionId, ToolChoice, ToolDefinition, ToolDisplay, ToolResult,
    UserMessage,
};
use philo_session::{MemorySessionStore, SessionStore, ToolResultOutcome};
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};
use support::gate::Gate;

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

fn poll_once<F: Future>(future: &mut std::pin::Pin<Box<F>>) -> Poll<F::Output> {
    let mut context = Context::from_waker(Waker::noop());
    future.as_mut().poll(&mut context)
}

fn config(max_tool_rounds: u32) -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "sys".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds,
        max_parallel_tool_calls: 1,
        operation_timeout: None,
        compaction: Default::default(),
    }
}

fn runtime(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<FakeTool>,
    max_tool_rounds: u32,
) -> AgentRuntime {
    AgentRuntime::with_tools(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(max_tool_rounds),
        tools,
    )
}

fn echo_definition() -> ToolDefinition {
    ToolDefinition::simple("echo", "echo", philo_agent_runtime::EffectClass::ReadOnly)
}

// --- Display channel: events only, never persisted (M10-002) -------------------

#[test]
fn display_reaches_events_but_never_the_session() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::text(&["done"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let display = ToolDisplay::new("full detail").with_fact("exit_code", "0");
    let tools = Arc::new(FakeTool::one(
        echo_definition(),
        FakeToolResult::success_with_display("model view", display.clone()),
    ));

    let mut handle = block_on(
        runtime(model, sessions.clone(), tools, 1)
            .prompt(SessionId::new("s"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { .. }
    ));

    // The event carries both channels, same-source-same-value.
    let mut completed = None;
    while let Some(event) = block_on(handle.next_event()) {
        if let AgentEvent::ToolExecutionCompleted {
            tool_name,
            result,
            display: event_display,
            ..
        } = event
        {
            completed = Some((tool_name, result, event_display));
        }
    }
    let (tool_name, result, event_display) = completed.expect("tool completed event");
    assert_eq!(tool_name, "echo");
    assert_eq!(result, ToolResult::success("model view"));
    assert_eq!(event_display, Some(display));

    // The durable fact equals the model channel; no display anywhere.
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

// --- Real-time consumption (M10-009) --------------------------------------------

#[test]
fn started_event_is_consumable_while_the_tool_is_still_executing() {
    let tool_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::text(&["done"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::one(
        echo_definition(),
        FakeToolResult::gated_success(&tool_gate, "ok"),
    ));
    let agent = runtime(model, sessions, tools, 1);

    // Acceptance returns immediately: the operation has not run yet.
    let mut handle = block_on(agent.prompt(SessionId::new("s"), UserMessage::new("hi"))).unwrap();

    // Consume events while the tool blocks: Started must arrive before the
    // tool completes (the gate is still closed).
    loop {
        let mut next = Box::pin(handle.next_event());
        match poll_once(&mut next) {
            Poll::Ready(Some(AgentEvent::ToolExecutionStarted {
                tool_name,
                arguments,
                ..
            })) => {
                assert_eq!(tool_name, "echo");
                assert_eq!(arguments, "{}");
                break;
            }
            Poll::Ready(Some(_)) => continue,
            Poll::Ready(None) => panic!("stream ended before Started"),
            Poll::Pending => {
                panic!("the event loop suspended before ToolExecutionStarted was published")
            }
        }
    }

    // No Completed yet: the barrier has not committed.
    let mut next = Box::pin(handle.next_event());
    assert!(
        poll_once(&mut next).is_pending(),
        "no completion event while the tool still executes"
    );
    drop(next);

    tool_gate.release();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { .. }
    ));
}

// --- Mid-run cancellation without a queue (M10-010) -----------------------------

#[test]
fn cancel_reaches_a_directly_admitted_running_operation() {
    let tool_gate = Gate::new();
    let model = Arc::new(FakeModel::new([ModelScript::tool_calls(&[
        (0, "call-1", "echo", "{}"),
        (1, "call-2", "echo", "{}"),
    ])]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::new(
        [echo_definition()],
        [
            FakeToolResult::gated_success(&tool_gate, "first done"),
            FakeToolResult::success("never runs"),
        ],
    ));
    let agent = runtime(model, sessions.clone(), tools, 1);

    let handle = block_on(agent.prompt(SessionId::new("s"), UserMessage::new("hi"))).unwrap();

    // Drive until the first call is executing (gate closed), then cancel.
    let mut wait = Box::pin(handle.wait());
    assert!(poll_once(&mut wait).is_pending(), "suspends inside call-1");
    handle.cancel();
    tool_gate.release();
    assert!(matches!(block_on(&mut wait), OperationOutcome::Cancelled));
    drop(wait);

    // M6 injection point 4: the executing call completed for real, the
    // never-executed call carries a Cancelled mark, all in one transaction.
    let view = block_on(sessions.context_view(&philo_session::SessionId::new("s"))).unwrap();
    let outcomes: Vec<_> = view
        .messages()
        .iter()
        .filter_map(|message| match message {
            philo_session::ContextMessage::ToolResult { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        outcomes,
        vec![
            ToolResultOutcome::Success {
                content: "first done".to_owned()
            },
            ToolResultOutcome::Cancelled,
        ]
    );
}

// --- Cancellation event payloads (M10-012) ---------------------------------------

#[test]
fn cancelled_batch_keeps_full_payloads_for_executed_calls_only() {
    let tool_gate = Gate::new();
    let model = Arc::new(FakeModel::new([ModelScript::tool_calls(&[
        (0, "call-1", "echo", r#"{"n":1}"#),
        (1, "call-2", "echo", r#"{"n":2}"#),
    ])]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::new(
        [echo_definition()],
        [
            FakeToolResult::gated_success(&tool_gate, "one"),
            FakeToolResult::success("never"),
        ],
    ));
    let agent = runtime(model, sessions, tools, 1);
    let mut handle = block_on(agent.prompt(SessionId::new("s"), UserMessage::new("hi"))).unwrap();

    let mut wait = Box::pin(handle.wait());
    assert!(poll_once(&mut wait).is_pending());
    handle.cancel();
    tool_gate.release();
    assert!(matches!(block_on(&mut wait), OperationOutcome::Cancelled));
    drop(wait);

    let mut started = Vec::new();
    let mut completed = Vec::new();
    while let Some(event) = block_on(handle.next_event()) {
        match event {
            AgentEvent::ToolExecutionStarted { tool_call_id, .. } => {
                started.push(tool_call_id.as_str().to_owned());
            }
            AgentEvent::ToolExecutionCompleted {
                tool_call_id,
                result,
                ..
            } => completed.push((tool_call_id.as_str().to_owned(), result)),
            _ => {}
        }
    }
    assert_eq!(started, ["call-1"], "the never-executed call has no events");
    assert_eq!(
        completed,
        vec![("call-1".to_owned(), ToolResult::success("one"))],
        "the executed call keeps its full payload"
    );
}

// --- tool_choice freezing (M10-008 runtime side) ---------------------------------

#[test]
fn tool_choice_freezes_into_every_snapshot_of_the_turn() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::text(&["done"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::one(
        echo_definition(),
        FakeToolResult::success("ok"),
    ));
    let mut runtime_config = config(1);
    runtime_config.generation.tool_choice = ToolChoice::Specific {
        name: "echo".to_owned(),
    };
    let agent = AgentRuntime::with_tools(
        model.clone(),
        sessions,
        Arc::new(SequentialIdSource::new()),
        runtime_config,
        tools,
    );

    let handle = block_on(agent.prompt(SessionId::new("s"), UserMessage::new("hi"))).unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { .. }
    ));

    let calls = model.calls();
    assert_eq!(calls.len(), 2);
    for snapshot in &calls {
        assert_eq!(
            snapshot.generation.tool_choice,
            ToolChoice::Specific {
                name: "echo".to_owned()
            },
            "frozen for the whole turn"
        );
    }
}

// --- Default tool_choice keeps the 0.8 shape --------------------------------------

#[test]
fn default_generation_config_uses_auto_tool_choice() {
    assert_eq!(GenerationConfig::default().tool_choice, ToolChoice::Auto);
}
