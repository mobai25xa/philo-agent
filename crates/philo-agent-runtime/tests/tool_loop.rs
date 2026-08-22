//! RUNTIME-002: 单轮工具循环与 barrier

mod support;

use std::sync::Arc;

use philo_agent_runtime::{
    AgentEvent, AgentFailureKind, GenerationConfig, ModelAssistantBlock, ModelEvent, ModelMessage,
    ModelToolCall, OperationOutcome, RuntimeConfig, SequentialIdSource, SessionId,
    SettlementDurability, ToolCallId, ToolDefinition, ToolPort, ToolRegistry, UserMessage,
};
use philo_session::{ContextMessage, MemorySessionStore, SessionStore, ToolResultOutcome};
use support::failing_session::{FailingSessionStore, FailurePlan};
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};

async fn collect_events(handle: &support::runtime::TestOp) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = handle.next_event().await {
        events.push(event);
    }
    events
}

fn config() -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "sys".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        // M2 semantics are the max_tool_rounds = 1 special case of the M3 loop.
        max_tool_rounds: 1,
        max_parallel_tool_calls: 1,
        operation_timeout: None,
        tool_cancel_grace: std::time::Duration::from_millis(300),
        compaction: Default::default(),
        recovery: Default::default(),
    }
}

fn echo_definition() -> ToolDefinition {
    ToolDefinition::simple("echo", "echo", philo_agent_runtime::EffectClass::ReadOnly)
}

fn definition(name: &str) -> ToolDefinition {
    ToolDefinition::simple(name, name, philo_agent_runtime::EffectClass::ReadOnly)
}

async fn runtime(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<dyn ToolPort>,
) -> support::runtime::TestRuntime {
    support::runtime::TestRuntime::with_tools(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(),
        tools,
    )
    .await
}

fn tool_loop_model(second: ModelScript) -> Arc<FakeModel> {
    Arc::new(FakeModel::new([
        ModelScript::tool_calls(&[(0, "call-a", "first", "{}"), (1, "call-b", "second", "{}")]),
        second,
    ]))
}

async fn success_handle(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<FakeTool>,
) -> (support::runtime::TestOp, Arc<FakeModel>, Arc<FakeTool>) {
    let handle = runtime(model.clone(), sessions, tools.clone())
        .await
        .prompt(SessionId::new("session"), UserMessage::new("hi"))
        .await;
    (handle, model, tools)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_fixture_executes_one_tool_loop() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::text(&["done"]),
    ]));
    let tools = Arc::new(FakeTool::one(
        echo_definition(),
        FakeToolResult::success("ok"),
    ));
    let sessions = Arc::new(MemorySessionStore::new());
    let handle = runtime(model.clone(), sessions.clone(), tools.clone())
        .await
        .prompt(SessionId::new("session"), UserMessage::new("hi"))
        .await;

    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "done"
    ));
    let events = collect_events(&handle).await;
    assert_eq!(model.call_count(), 2);
    assert_eq!(tools.invocation_count(), 1);
    assert!(model.calls()[1].tools.is_empty());
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolExecutionStarted { index: 0, .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolExecutionCompleted { index: 0, .. }))
    );
    let view = sessions
        .context_view(&philo_session::SessionId::new("session"))
        .await
        .unwrap();
    assert!(matches!(
        view.messages()[1],
        ContextMessage::AssistantToolCalls { .. }
    ));
    assert!(matches!(
        view.messages()[2],
        ContextMessage::ToolResult { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_failing_store_targets_context_read_and_barrier_a() {
    for plan in [FailurePlan::context_read_at(1), FailurePlan::commit_at(1)] {
        let model = Arc::new(FakeModel::succeeds(&["unused"]));
        let tools = Arc::new(FakeTool::new([], []));
        let store = Arc::new(FailingSessionStore::memory(plan));
        let handle = runtime(model.clone(), store, tools)
            .await
            .prompt(SessionId::new("session"), UserMessage::new("hi"))
            .await;
        assert!(matches!(
            handle.wait().await,
            OperationOutcome::Failed { .. }
        ));
        assert_eq!(model.call_count(), 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_failing_store_can_fail_barrier_e_only() {
    let model = Arc::new(FakeModel::start_fails("offline"));
    let tools = Arc::new(FakeTool::new([], []));
    let store = Arc::new(FailingSessionStore::memory(FailurePlan::commit_at(2)));
    let handle = runtime(model.clone(), store.clone(), tools)
        .await
        .prompt(SessionId::new("session"), UserMessage::new("hi"))
        .await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Failed {
            durability: philo_agent_runtime::SettlementDurability::Unconfirmed,
            ..
        }
    ));
    assert_eq!(model.call_count(), 1);
    assert_eq!(store.commit_count(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_tool_success() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{", "}"]),
        ModelScript::text(&["done"]),
    ]));
    let tools = Arc::new(FakeTool::one(
        definition("echo"),
        FakeToolResult::success("ok"),
    ));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, model, tools) = success_handle(model, sessions.clone(), tools).await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "done"
    ));
    assert_eq!(model.call_count(), 2);
    assert_eq!(tools.invocation_count(), 1);
    assert!(model.calls()[1].tools.is_empty());
    let view = sessions
        .context_view(&philo_session::SessionId::new("session"))
        .await
        .unwrap();
    assert!(matches!(
        view.messages()[1],
        ContextMessage::AssistantToolCalls { .. }
    ));
    assert!(matches!(
        view.messages()[2],
        ContextMessage::ToolResult {
            outcome: ToolResultOutcome::Success { .. },
            ..
        }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_tool_source_order() {
    let model = tool_loop_model(ModelScript::text(&["done"]));
    let tools = Arc::new(FakeTool::new(
        [definition("first"), definition("second")],
        [
            FakeToolResult::success("one"),
            FakeToolResult::success("two"),
        ],
    ));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, model, tools) = success_handle(model, sessions.clone(), tools).await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { .. }
    ));
    let events = collect_events(&handle).await;
    let calls = tools.invocations();
    assert_eq!(
        calls.iter().map(|call| call.call_id()).collect::<Vec<_>>(),
        ["call-a", "call-b"]
    );
    let started = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionStarted {
                tool_call_id,
                index,
                ..
            } => Some((tool_call_id.as_str(), *index)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let completed = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionCompleted {
                tool_call_id,
                index,
                ..
            } => Some((tool_call_id.as_str(), *index)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(started, [("call-a", 0), ("call-b", 1)]);
    assert_eq!(completed, [("call-a", 0), ("call-b", 1)]);
    let view = sessions
        .context_view(&philo_session::SessionId::new("session"))
        .await
        .unwrap();
    let result_ids = view
        .messages()
        .iter()
        .filter_map(|message| match message {
            ContextMessage::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(result_ids, ["call-a", "call-b"]);
    let second_messages = &model.calls()[1].messages;
    let model_result_ids = second_messages
        .iter()
        .filter_map(|message| match message {
            ModelMessage::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(model_result_ids, ["call-a", "call-b"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_errors_continue_to_final_answer() {
    let unknown = Arc::new(ToolRegistry::empty());
    assert_tool_error_continues("missing", "{}", unknown, "unknown_tool").await;

    let invalid = Arc::new(
        ToolRegistry::builder()
            .register(definition("echo"), |_arguments| async {
                philo_agent_runtime::RichToolResult::success("unused")
            })
            .unwrap()
            .build(),
    );
    assert_tool_error_continues("echo", "[]", invalid, "invalid_arguments").await;

    let business = Arc::new(
        ToolRegistry::builder()
            .register(definition("echo"), |_arguments| async {
                philo_agent_runtime::RichToolResult::error("domain_error", "bad input")
            })
            .unwrap()
            .build(),
    );
    assert_tool_error_continues("echo", "{}", business, "domain_error").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn barrier_b_failure_prevents_tool_execution() {
    let model = Arc::new(FakeModel::new([ModelScript::tool_call(
        0,
        Some("call-1"),
        Some("echo"),
        &["{}"],
    )]));
    let tools = Arc::new(FakeTool::one(
        definition("echo"),
        FakeToolResult::success("unused"),
    ));
    let store = Arc::new(FailingSessionStore::memory(FailurePlan::commit_at(2)));
    let (handle, model, tools) = success_handle(model, store, tools).await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Failed {
            durability: SettlementDurability::Confirmed,
            ..
        }
    ));
    assert_eq!(model.call_count(), 1);
    assert_eq!(tools.invocation_count(), 0);
    let events = collect_events(&handle).await;
    assert!(!events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolBatchRequested { .. } | AgentEvent::ToolExecutionStarted { .. }
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn barrier_c_failure_prevents_second_model_call() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::text(&["must not run"]),
    ]));
    let tools = Arc::new(FakeTool::one(
        definition("echo"),
        FakeToolResult::success("ok"),
    ));
    let store = Arc::new(FailingSessionStore::memory(FailurePlan::commit_at(3)));
    let (handle, model, tools) = success_handle(model, store, tools).await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Failed {
            durability: SettlementDurability::Confirmed,
            ..
        }
    ));
    assert_eq!(model.call_count(), 1);
    assert_eq!(tools.invocation_count(), 1);
    assert!(
        !collect_events(&handle)
            .await
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolExecutionCompleted { .. }))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_infrastructure_failure_settles_failed() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::text(&["must not run"]),
    ]));
    let tools = Arc::new(FakeTool::one(
        definition("echo"),
        FakeToolResult::infrastructure_error("worker unavailable"),
    ));
    let (handle, model, tools) =
        success_handle(model, Arc::new(MemorySessionStore::new()), tools).await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Failed { failure, durability: SettlementDurability::Confirmed } if failure.kind() == AgentFailureKind::ToolExecution
    ));
    assert_eq!(model.call_count(), 1);
    assert_eq!(tools.invocation_count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_tool_call_is_rejected() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::tool_call(0, Some("call-2"), Some("echo"), &["{}"]),
    ]));
    let tools = Arc::new(FakeTool::one(
        definition("echo"),
        FakeToolResult::success("ok"),
    ));
    let (handle, model, tools) =
        success_handle(model, Arc::new(MemorySessionStore::new()), tools).await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Failed { failure, durability: SettlementDurability::Confirmed } if failure.kind() == AgentFailureKind::InvalidModelOutput
    ));
    assert_eq!(model.call_count(), 2);
    assert_eq!(tools.invocation_count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_commit_failure_prevents_success() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::text(&["done"]),
    ]));
    let tools = Arc::new(FakeTool::one(
        definition("echo"),
        FakeToolResult::success("ok"),
    ));
    let store = Arc::new(FailingSessionStore::memory(FailurePlan::commit_at(4)));
    let (handle, model, tools) = success_handle(model, store, tools).await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Failed {
            durability: SettlementDurability::Confirmed,
            ..
        }
    ));
    assert_eq!(model.call_count(), 2);
    assert_eq!(tools.invocation_count(), 1);
    assert!(
        !collect_events(&handle)
            .await
            .iter()
            .any(|event| matches!(event, AgentEvent::AssistantMessageCompleted { .. }))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_session_failure_is_unconfirmed() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::text(&["done"]),
    ]));
    let tools = Arc::new(FakeTool::one(
        definition("echo"),
        FakeToolResult::success("ok"),
    ));
    let store = Arc::new(FailingSessionStore::memory(
        FailurePlan::persistent_commit_at(4),
    ));
    let (handle, model, tools) = success_handle(model, store, tools).await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Failed {
            durability: SettlementDurability::Unconfirmed,
            ..
        }
    ));
    assert_eq!(model.call_count(), 2);
    assert_eq!(tools.invocation_count(), 1);
    let events = collect_events(&handle).await;
    assert!(!events.iter().any(|event| matches!(
        event,
        AgentEvent::AssistantMessageCompleted { .. } | AgentEvent::TurnFailed { .. }
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_turn_reads_prior_tool_exchange() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::text(&["first answer"]),
        ModelScript::text(&["second answer"]),
    ]));
    let tools = Arc::new(FakeTool::one(
        definition("echo"),
        FakeToolResult::success("tool"),
    ));
    let sessions = Arc::new(MemorySessionStore::new());
    let runtime = runtime(model.clone(), sessions, tools).await;
    assert!(matches!(
        runtime
            .prompt(SessionId::new("session"), UserMessage::new("first"))
            .await
            .wait()
            .await,
        OperationOutcome::Succeeded { .. }
    ));
    assert!(matches!(
        runtime
            .prompt(SessionId::new("session"), UserMessage::new("second"))
            .await
            .wait()
            .await,
        OperationOutcome::Succeeded { .. }
    ));
    let messages = &model.calls()[2].messages;
    assert!(messages.iter().any(|message| {
        matches!(
            message,
            ModelMessage::Assistant { blocks }
                if blocks
                    .iter()
                    .any(|block| matches!(block, ModelAssistantBlock::ToolCall(_)))
        )
    }));
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, ModelMessage::ToolResult { .. }))
    );
    assert!(messages.iter().any(|message| matches!(
        message,
        ModelMessage::Assistant { blocks }
            if matches!(
                blocks.as_slice(),
                [ModelAssistantBlock::Text { text }] if text == "first answer"
            )
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_event_is_emitted_once() {
    let success = success_handle(
        Arc::new(FakeModel::succeeds(&["done"])),
        Arc::new(MemorySessionStore::new()),
        Arc::new(FakeTool::new([], [])),
    )
    .await
    .0;
    let confirmed_failure = success_handle(
        Arc::new(FakeModel::start_fails("offline")),
        Arc::new(MemorySessionStore::new()),
        Arc::new(FakeTool::new([], [])),
    )
    .await
    .0;
    let unconfirmed_failure = success_handle(
        Arc::new(FakeModel::new([
            ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
            ModelScript::text(&["done"]),
        ])),
        Arc::new(FailingSessionStore::memory(
            FailurePlan::persistent_commit_at(4),
        )),
        Arc::new(FakeTool::one(
            definition("echo"),
            FakeToolResult::success("ok"),
        )),
    )
    .await
    .0;
    for handle in [success, confirmed_failure, unconfirmed_failure] {
        let events = collect_events(&handle).await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::OperationSettled { .. }))
                .count(),
            1
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::OperationSettled { .. })
        ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_answer_regression() {
    let model = Arc::new(FakeModel::succeeds(&["hello"]));
    let tools = Arc::new(FakeTool::new([], []));
    let (handle, model, tools) =
        success_handle(model, Arc::new(MemorySessionStore::new()), tools).await;
    assert!(
        matches!(handle.wait().await, OperationOutcome::Succeeded { assistant } if assistant.content() == "hello")
    );
    assert_eq!(model.call_count(), 1);
    assert_eq!(tools.invocation_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_text_and_tool_call_continues_the_tool_loop() {
    let model = Arc::new(FakeModel::new([
        ModelScript::Events(vec![
            Ok(ModelEvent::TextDelta("calling echo".to_owned())),
            Ok(ModelEvent::ToolCallDelta {
                index: 0,
                id: Some("call-1".to_owned()),
                name: Some("echo".to_owned()),
                arguments: "{}".to_owned(),
            }),
            Ok(ModelEvent::Completed {
                blocks: vec![
                    ModelAssistantBlock::Text {
                        text: "calling echo".to_owned(),
                    },
                    ModelAssistantBlock::ToolCall(ModelToolCall {
                        tool_call_id: ToolCallId::new("call-1"),
                        name: "echo".to_owned(),
                        arguments: "{}".to_owned(),
                    }),
                ],
            }),
        ]),
        ModelScript::text(&["done"]),
    ]));
    let tools = Arc::new(FakeTool::one(
        definition("echo"),
        FakeToolResult::success("ok"),
    ));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, model, tools) = success_handle(model, sessions.clone(), tools).await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "done"
    ));
    assert_eq!(model.call_count(), 2);
    assert_eq!(tools.invocation_count(), 1);
    let events = collect_events(&handle).await;
    assert!(
        events.iter().any(|event| {
            matches!(event, AgentEvent::TextDelta { delta } if delta.contains("calling echo"))
        }),
        "mixed-turn text must remain visible after sealed-stream merge: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolBatchRequested { .. }))
    );
    let view = sessions
        .context_view(&philo_session::SessionId::new("session"))
        .await
        .unwrap();
    assert!(matches!(
        view.messages()[1],
        ContextMessage::AssistantToolCalls { .. }
    ));
}

async fn assert_tool_error_continues(
    name: &str,
    arguments: &str,
    tools: Arc<dyn ToolPort>,
    expected_code: &str,
) {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some(name), &[arguments]),
        ModelScript::text(&["recovered"]),
    ]));
    let handle = runtime(model.clone(), Arc::new(MemorySessionStore::new()), tools)
        .await
        .prompt(SessionId::new("session"), UserMessage::new("hi"))
        .await;
    assert!(
        matches!(handle.wait().await, OperationOutcome::Succeeded { assistant } if assistant.content() == "recovered")
    );
    assert!(matches!(
        model.calls()[1].messages.last(),
        Some(ModelMessage::ToolResult { outcome: philo_agent_runtime::ModelToolResultOutcome::Error { code, .. }, .. }) if code == expected_code
    ));
}
