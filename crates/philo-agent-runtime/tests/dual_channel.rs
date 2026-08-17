//! RUNTIME-008: dual-channel absorption, real-time event consumption,
//! mid-run cancellation reachability, and tool_choice freezing.

mod support;

use std::sync::Arc;
use std::time::Duration;

use philo_agent_runtime::{
    AgentEvent, GenerationConfig, OperationOutcome, OperationPhase, RunningToolBatchPhase,
    RuntimeConfig, SequentialIdSource, SessionId, ToolChoice, ToolDefinition, ToolDisplay,
    ToolResult, UserMessage,
};
use philo_session::{MemorySessionStore, SessionStore, ToolResultOutcome};
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};
use support::gate::Gate;

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

async fn runtime(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<FakeTool>,
    max_tool_rounds: u32,
) -> support::runtime::TestRuntime {
    support::runtime::TestRuntime::with_tools(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(max_tool_rounds),
        tools,
    )
    .await
}

fn echo_definition() -> ToolDefinition {
    ToolDefinition::simple("echo", "echo", philo_agent_runtime::EffectClass::ReadOnly)
}

// --- Display channel: events only, never persisted (M10-002) -------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn display_reaches_events_but_never_the_session() {
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

    let handle = runtime(model, sessions.clone(), tools, 1)
        .await
        .prompt(SessionId::new("s"), UserMessage::new("hi"))
        .await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { .. }
    ));

    // The event carries both channels, same-source-same-value.
    let mut completed = None;
    while let Some(event) = handle.next_event().await {
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

// --- Real-time consumption (M10-009) --------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn started_event_is_consumable_while_the_tool_is_still_executing() {
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
    let agent = runtime(model, sessions, tools, 1).await;

    // Acceptance returns immediately: the operation has not run yet.
    let handle = agent
        .prompt(SessionId::new("s"), UserMessage::new("hi"))
        .await;

    // Consume events while the tool blocks: Started must arrive before the
    // tool completes (the gate is still closed).
    loop {
        match handle.next_event().await {
            Some(AgentEvent::ToolExecutionStarted {
                tool_name,
                arguments,
                ..
            }) => {
                assert_eq!(tool_name, "echo");
                assert_eq!(arguments, "{}");
                break;
            }
            Some(_) => continue,
            None => panic!("stream ended before Started"),
        }
    }

    // No Completed yet: the barrier has not committed.
    match tokio::time::timeout(Duration::from_millis(80), handle.next_event()).await {
        Ok(Some(AgentEvent::ToolExecutionCompleted { .. })) => {
            panic!("no completion event while the tool still executes")
        }
        Ok(Some(other)) => panic!("unexpected event while the tool still executes: {other:?}"),
        Ok(None) => panic!("stream ended while the tool still executes"),
        Err(_) => {}
    }

    tool_gate.release();
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { .. }
    ));
}

// --- Mid-run cancellation without a queue (M10-010) -----------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_reaches_a_directly_admitted_running_operation() {
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
    let agent = runtime(model, sessions.clone(), tools, 1).await;

    let handle = agent
        .prompt(SessionId::new("s"), UserMessage::new("hi"))
        .await;

    // Drive until the first call is executing (gate closed), then cancel.
    handle
        .wait_until_phase(|phase| {
            matches!(
                phase,
                OperationPhase::RunningToolBatch(RunningToolBatchPhase::Executing {
                    in_flight: 1,
                    completed: 0,
                })
            )
        })
        .await;
    handle.cancel().await;
    tool_gate.release();
    assert!(matches!(handle.wait().await, OperationOutcome::Cancelled));

    // M6 injection point 4: the executing call completed for real, the
    // never-executed call carries a Cancelled mark, all in one transaction.
    let view = sessions
        .context_view(&philo_session::SessionId::new("s"))
        .await
        .unwrap();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_batch_keeps_full_payloads_for_executed_calls_only() {
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
    let agent = runtime(model, sessions, tools, 1).await;
    let handle = agent
        .prompt(SessionId::new("s"), UserMessage::new("hi"))
        .await;

    handle
        .wait_until_phase(|phase| {
            matches!(
                phase,
                OperationPhase::RunningToolBatch(RunningToolBatchPhase::Executing { .. })
            )
        })
        .await;
    handle.cancel().await;
    tool_gate.release();
    assert!(matches!(handle.wait().await, OperationOutcome::Cancelled));

    let mut started = Vec::new();
    let mut completed = Vec::new();
    while let Some(event) = handle.next_event().await {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_choice_freezes_into_every_snapshot_of_the_turn() {
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
    let agent = support::runtime::TestRuntime::with_tools(
        model.clone(),
        sessions,
        Arc::new(SequentialIdSource::new()),
        runtime_config,
        tools,
    )
    .await;

    let handle = agent
        .prompt(SessionId::new("s"), UserMessage::new("hi"))
        .await;
    assert!(matches!(
        handle.wait().await,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_generation_config_uses_auto_tool_choice() {
    assert_eq!(GenerationConfig::default().tool_choice, ToolChoice::Auto);
}
