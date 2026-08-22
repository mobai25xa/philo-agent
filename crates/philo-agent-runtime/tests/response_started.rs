//! RUNTIME-004: ModelEvent/AgentEvent ResponseStarted passthrough.

mod support;

use std::sync::Arc;

use philo_agent_runtime::{
    AgentEvent, AgentFailureKind, GenerationConfig, ModelAssistantBlock, ModelError, ModelEvent,
    OperationOutcome, RuntimeConfig, SequentialIdSource, SessionId, ToolDefinition, UserMessage,
};
use philo_session::MemorySessionStore;
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};

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
        recovery: Default::default(),
    }
}

async fn collect_events(handle: &support::runtime::TestOp) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = handle.next_event().await {
        events.push(event);
    }
    events
}

async fn direct_runtime(model: Arc<FakeModel>) -> support::runtime::TestRuntime {
    support::runtime::TestRuntime::new(
        model,
        Arc::new(MemorySessionStore::new()),
        Arc::new(SequentialIdSource::new()),
        config(0),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_started_is_forwarded_with_metadata() {
    let model =
        Arc::new(FakeModel::new([ModelScript::text(&["hi"])
            .with_response_started(Some("real-model-001"), Some("resp-1"))]));
    let handle = direct_runtime(model)
        .await
        .prompt(SessionId::new("session"), UserMessage::new("hello"))
        .await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { .. }
    ));

    let events = collect_events(&handle).await;
    let call_started = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ModelCallStarted { .. }))
        .expect("model call started");
    let response_started = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ModelResponseStarted { .. }))
        .expect("model response started");
    let first_delta = events
        .iter()
        .position(|event| matches!(event, AgentEvent::TextDelta { .. }))
        .expect("text delta");
    let completed = events
        .iter()
        .position(|event| matches!(event, AgentEvent::AssistantMessageCompleted { .. }))
        .expect("assistant message completed");
    assert!(call_started < response_started);
    assert!(response_started < first_delta);
    assert!(response_started < completed);

    let AgentEvent::ModelResponseStarted {
        model_call_id,
        response_model,
        response_id,
    } = &events[response_started]
    else {
        unreachable!()
    };
    let AgentEvent::ModelCallStarted {
        model_call_id: started_id,
    } = &events[call_started]
    else {
        unreachable!()
    };
    assert_eq!(model_call_id, started_id);
    assert_eq!(response_model.as_deref(), Some("real-model-001"));
    assert_eq!(response_id.as_deref(), Some("resp-1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_started_fields_may_be_absent() {
    let model = Arc::new(FakeModel::new([
        ModelScript::text(&["ok"]).with_response_started(None, None)
    ]));
    let handle = direct_runtime(model)
        .await
        .prompt(SessionId::new("session"), UserMessage::new("hello"))
        .await;
    let events = collect_events(&handle).await;
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ModelResponseStarted {
            response_model: None,
            response_id: None,
            ..
        }
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_model_call_forwards_its_own_response_started() {
    let definition =
        ToolDefinition::simple("echo", "echo", philo_agent_runtime::EffectClass::ReadOnly);
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"])
            .with_response_started(Some("m"), Some("resp-1")),
        ModelScript::text(&["done"]).with_response_started(Some("m"), Some("resp-2")),
    ]));
    let tools = Arc::new(FakeTool::one(definition, FakeToolResult::success("one")));
    let runtime = support::runtime::TestRuntime::with_tools(
        model,
        Arc::new(MemorySessionStore::new()),
        Arc::new(SequentialIdSource::new()),
        config(1),
        tools,
    )
    .await;
    let handle = runtime
        .prompt(SessionId::new("session"), UserMessage::new("hi"))
        .await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { .. }
    ));

    let events = collect_events(&handle).await;
    let mut current_call: Option<String> = None;
    let mut observed = Vec::new();
    for event in &events {
        match event {
            AgentEvent::ModelCallStarted { model_call_id } => {
                current_call = Some(model_call_id.as_str().to_owned());
            }
            AgentEvent::ModelResponseStarted {
                model_call_id,
                response_id,
                ..
            } => {
                assert_eq!(
                    Some(model_call_id.as_str().to_owned()),
                    current_call,
                    "response started must follow its own model call"
                );
                observed.push(response_id.clone());
            }
            _ => {}
        }
    }
    assert_eq!(
        observed,
        vec![Some("resp-1".to_owned()), Some("resp-2".to_owned())]
    );

    // The first round's passthrough precedes that round's tool batch events.
    let first_response = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ModelResponseStarted { .. }))
        .expect("first response started");
    let batch_requested = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ToolBatchRequested { .. }))
        .expect("tool batch requested");
    assert!(first_response < batch_requested);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streams_without_response_started_keep_baseline_behavior() {
    let model = Arc::new(FakeModel::succeeds(&["plain"]));
    let handle = direct_runtime(model)
        .await
        .prompt(SessionId::new("session"), UserMessage::new("hello"))
        .await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "plain"
    ));
    let events = collect_events(&handle).await;
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, AgentEvent::ModelResponseStarted { .. })),
        "absent event must not be synthesized"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_response_started_fails_the_operation() {
    let model = Arc::new(FakeModel::new([ModelScript::Events(vec![
        Ok(ModelEvent::ResponseStarted {
            response_model: None,
            response_id: Some("resp-1".to_owned()),
        }),
        Ok(ModelEvent::ResponseStarted {
            response_model: None,
            response_id: Some("resp-2".to_owned()),
        }),
        Ok(ModelEvent::TextDelta("hi".to_owned())),
        Ok(ModelEvent::Completed {
            blocks: vec![ModelAssistantBlock::Text {
                text: "hi".to_owned(),
            }],
        }),
    ])]));
    let handle = direct_runtime(model)
        .await
        .prompt(SessionId::new("session"), UserMessage::new("hello"))
        .await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Failed { failure, .. }
            if failure.kind() == AgentFailureKind::InvalidModelOutput
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_started_after_completed_fails_the_operation() {
    let model = Arc::new(FakeModel::new([ModelScript::Events(vec![
        Ok(ModelEvent::TextDelta("hi".to_owned())),
        Ok(ModelEvent::Completed {
            blocks: vec![ModelAssistantBlock::Text {
                text: "hi".to_owned(),
            }],
        }),
        Ok(ModelEvent::ResponseStarted {
            response_model: None,
            response_id: None,
        }),
    ])]));
    let handle = direct_runtime(model)
        .await
        .prompt(SessionId::new("session"), UserMessage::new("hello"))
        .await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Failed { failure, .. }
            if failure.kind() == AgentFailureKind::InvalidModelOutput
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failing_stream_after_response_started_still_fails_normally() {
    let model = Arc::new(FakeModel::new([ModelScript::Events(vec![
        Ok(ModelEvent::ResponseStarted {
            response_model: Some("m".to_owned()),
            response_id: None,
        }),
        Err(ModelError::new("stream broke")),
    ])]));
    let handle = direct_runtime(model)
        .await
        .prompt(SessionId::new("session"), UserMessage::new("hello"))
        .await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Failed { failure, .. }
            if failure.kind() == AgentFailureKind::ModelCall && failure.message() == "stream broke"
    ));
    let events = collect_events(&handle).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ModelResponseStarted { .. })),
        "transient observation before the failure is still published"
    );
}
