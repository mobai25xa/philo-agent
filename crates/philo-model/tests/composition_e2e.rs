//! INTEGRATION-004: end-to-end composition through production public APIs.
//!
//! AgentRuntime + philo-model (stub transport) + philo-tools-std + a real
//! ToolRegistry + MemorySessionStore. live smoke 在 `live_smoke.rs`。

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use philo_agent_runtime::{
    AgentEvent, AgentFailureKind, AgentRuntime, ChannelBounds, GenerationConfig, OperationOutcome,
    OperationSpec, RuntimeConfig, RuntimeDeps, SequentialIdSource, SessionId, SettlementDurability,
    ToolRegistry, UserMessage,
};
use philo_session::{
    ContextMessage, MemorySessionStore, OperationOutcome as SessionOperationOutcome,
    SessionAssistantBlock, SessionEntryKind, SessionStore, SessionToolCall, SessionToolResult,
    SessionTransaction, ToolResultOutcome, TurnOutcome,
};
use philo_tools_std::ReadTool;
use support::{
    StubResponse, StubTransport, adapter_over, drain_until_settled, generation, sse, text_sse,
};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-m4-e2e-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn file(&self, relative: &str, content: &str) {
        std::fs::write(self.path.join(relative), content).expect("write fixture file");
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn compose_generation(
    transport: StubTransport,
    root: &TempRoot,
    max_tool_rounds: u32,
) -> Arc<philo_agent_runtime::RuntimeGeneration> {
    let registry = ToolRegistry::builder()
        .register(ReadTool::definition(), ReadTool::new(&root.path))
        .expect("register read tool")
        .build();
    generation(
        Arc::new(adapter_over(transport)),
        Arc::new(registry),
        RuntimeConfig {
            system_prompt: "You are the M4 integration assistant.".to_owned(),
            model_target: "stub-model".to_owned(),
            generation: GenerationConfig {
                max_output_tokens: 256,
                temperature: 0.0,
                reasoning_effort: None,
                tool_choice: philo_agent_runtime::ToolChoice::Auto,
            },
            max_tool_rounds,
            max_parallel_tool_calls: 1,
            operation_timeout: None,
            tool_cancel_grace: std::time::Duration::from_millis(300),
            compaction: Default::default(),
            recovery: Default::default(),
        },
    )
}

/// A scripted single-tool-call round: `read` with fragmented arguments.
fn tool_round_sse(response_id: &str, call_id: &str, path: &str) -> Vec<u8> {
    let head = format!(
        r#"{{"id":"{response_id}","object":"chat.completion.chunk","model":"stub-gpt","choices":[{{"index":0,"delta":{{"role":"assistant"}},"finish_reason":null}}]}}"#
    );
    let start = format!(
        r#"{{"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"{call_id}","type":"function","function":{{"name":"read","arguments":""}}}}]}},"finish_reason":null}}]}}"#
    );
    let args_head = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]},"finish_reason":null}]}"#.to_owned();
    let args_tail = format!(
        r#"{{"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"function":{{"arguments":"\"{path}\"}}"}}}}]}},"finish_reason":null}}]}}"#
    );
    let finish = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#.to_owned();
    sse(&[&head, &start, &args_head, &args_tail, &finish, "[DONE]"])
}

fn position(events: &[AgentEvent], matcher: impl Fn(&AgentEvent) -> bool, what: &str) -> usize {
    events
        .iter()
        .position(matcher)
        .unwrap_or_else(|| panic!("missing event: {what}"))
}

/// M4-001 + M4-002: a real multi-round tool loop over the real adapter and
/// registry persists 0.4-contract facts and publishes the passthrough events.
#[tokio::test(flavor = "multi_thread")]
async fn multi_round_tool_loop_with_real_adapter_and_registry() {
    let root = TempRoot::new();
    root.file("hello.txt", "philo agent M4 content");
    let transport = StubTransport::new([
        StubResponse::Sse(tool_round_sse("resp-1", "call-1", "hello.txt")),
        StubResponse::Sse(text_sse(
            "resp-2",
            "stub-gpt",
            &["The file says: ", "philo"],
        )),
    ]);
    let sessions = Arc::new(MemorySessionStore::new());
    let generation = compose_generation(transport.clone(), &root, 2);
    let parts = AgentRuntime::start(RuntimeDeps {
        sessions: sessions.clone(),
        ids: Arc::new(SequentialIdSource::new()),
        bounds: ChannelBounds::default(),
    })
    .expect("start runtime");
    let handle = parts.handle;
    let mut sub = parts.events;

    let accepted = handle
        .submit(OperationSpec {
            session_id: SessionId::new("m4-001"),
            user_message: UserMessage::new("read hello.txt"),
            generation,
            service_request_id: None,
        })
        .await
        .expect("submit accepted");
    let (events, outcome) = drain_until_settled(&mut sub, &accepted.operation_id).await;
    assert!(matches!(
        outcome,
        OperationOutcome::Succeeded { assistant }
            if assistant.content() == "The file says: philo"
    ));

    // Event order per the 0.4 contract plus the RUNTIME-004 passthrough.
    let started = position(
        &events,
        |event| matches!(event, AgentEvent::OperationStarted { .. }),
        "operation started",
    );
    let first_call = position(
        &events,
        |event| matches!(event, AgentEvent::ModelCallStarted { .. }),
        "first model call",
    );
    let first_response = position(
        &events,
        |event| {
            matches!(
                event,
                AgentEvent::ModelResponseStarted { response_id: Some(id), .. } if id == "resp-1"
            )
        },
        "first response started",
    );
    let batch = position(
        &events,
        |event| matches!(event, AgentEvent::ToolBatchRequested { .. }),
        "tool batch requested",
    );
    let execution_completed = position(
        &events,
        |event| matches!(event, AgentEvent::ToolExecutionCompleted { .. }),
        "tool execution completed",
    );
    let second_response = position(
        &events,
        |event| {
            matches!(
                event,
                AgentEvent::ModelResponseStarted { response_id: Some(id), .. } if id == "resp-2"
            )
        },
        "second response started",
    );
    let message_completed = position(
        &events,
        |event| matches!(event, AgentEvent::AssistantMessageCompleted { .. }),
        "assistant message completed",
    );
    let settled = position(
        &events,
        |event| matches!(event, AgentEvent::OperationSettled { .. }),
        "operation settled",
    );
    assert!(started < first_call);
    assert!(first_call < first_response);
    assert!(first_response < batch);
    assert!(batch < execution_completed);
    assert!(execution_completed < second_response);
    assert!(second_response < message_completed);
    assert!(message_completed < settled);

    // Durable facts follow the 0.4 contract: user, tool calls, result, final.
    let view = sessions
        .context_view(&philo_session::SessionId::new("m4-001"))
        .await
        .expect("context view");
    let messages = view.messages();
    assert_eq!(messages.len(), 4);
    assert!(matches!(
        &messages[0],
        ContextMessage::User { parts }
            if parts == &philo_session::SessionUserPart::text_parts("read hello.txt")
    ));
    let ContextMessage::AssistantToolCalls { blocks, .. } = &messages[1] else {
        panic!("expected persisted tool calls, got {:?}", messages[1]);
    };
    let calls: Vec<_> = blocks
        .iter()
        .filter_map(|block| match block {
            SessionAssistantBlock::ToolCall(call) => Some(call),
            SessionAssistantBlock::Text { .. } => None,
        })
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name(), "read");
    assert_eq!(calls[0].arguments(), r#"{"path":"hello.txt"}"#);
    let ContextMessage::ToolResult { outcome, .. } = &messages[2] else {
        panic!("expected persisted tool result, got {:?}", messages[2]);
    };
    // M10 read output shape: line-number prefixes; the durable fact equals
    // the tool's finalized model-channel text.
    assert_eq!(
        outcome,
        &ToolResultOutcome::Success {
            content: "    1|philo agent M4 content\n".to_owned()
        }
    );
    assert!(matches!(
        &messages[3],
        ContextMessage::Assistant { blocks }
            if blocks
                == &vec![SessionAssistantBlock::Text {
                    text: "The file says: philo".to_owned(),
                }]
    ));

    // The second model call replayed the tool transcript through the adapter.
    let bodies = transport.request_bodies();
    assert_eq!(bodies.len(), 2);
    let replay = &bodies[1];
    assert_eq!(
        replay["messages"][2]["tool_calls"][0]["function"]["arguments"],
        r#"{"path":"hello.txt"}"#
    );
    assert_eq!(
        replay["messages"][3]["content"][0]["text"],
        "    1|philo agent M4 content\n"
    );
}

/// M4-003: infrastructure failures normalize to ModelError and settle the
/// operation through the existing confirmed failure path. The default
/// recovery policy retries the connect fault until its budget exhausts, so
/// every attempt is scripted.
#[tokio::test(flavor = "multi_thread")]
async fn transport_failure_settles_failed_with_durable_facts() {
    let root = TempRoot::new();
    let transport = StubTransport::new([
        StubResponse::ConnectError,
        StubResponse::ConnectError,
        StubResponse::ConnectError,
        StubResponse::ConnectError,
    ]);
    let sessions = Arc::new(MemorySessionStore::new());
    let generation = compose_generation(transport, &root, 2);
    let parts = AgentRuntime::start(RuntimeDeps {
        sessions: sessions.clone(),
        ids: Arc::new(SequentialIdSource::new()),
        bounds: ChannelBounds::default(),
    })
    .expect("start runtime");
    let handle = parts.handle;
    let mut sub = parts.events;

    let accepted = handle
        .submit(OperationSpec {
            session_id: SessionId::new("m4-003"),
            user_message: UserMessage::new("hello"),
            generation,
            service_request_id: None,
        })
        .await
        .expect("submit accepted");
    let (events, outcome) = drain_until_settled(&mut sub, &accepted.operation_id).await;
    let OperationOutcome::Failed {
        failure,
        durability,
    } = outcome
    else {
        panic!("expected failure, got {outcome:?}");
    };
    assert_eq!(failure.kind(), AgentFailureKind::ModelCall);
    assert!(
        failure.message().contains("Transport"),
        "normalized kind summary expected: {}",
        failure.message()
    );
    assert_eq!(durability, SettlementDurability::Confirmed);

    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnFailed { .. })),
        "confirmed failure publishes TurnFailed"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::OperationSettled {
            durability: SettlementDurability::Confirmed,
            ..
        }
    )));
}

/// M4-004: a turn whose durable history contains invalid raw arguments is
/// replayed with the `{}` placeholder and the next call succeeds.
#[tokio::test(flavor = "multi_thread")]
async fn history_with_invalid_raw_arguments_replays_degraded() {
    let root = TempRoot::new();
    let session_id = philo_session::SessionId::new("m4-004");
    let sessions = Arc::new(MemorySessionStore::new());

    // Seed a completed prior turn with invalid raw tool arguments, mirroring
    // the runtime's barrier structure (start, batch, results, settlement).
    let turn = philo_session::TurnId::new("seed-turn");
    let operation = philo_session::OperationId::new("seed-operation");
    let start = sessions
        .commit(SessionTransaction::linear(
            session_id.clone(),
            philo_session::SessionRevision::ZERO,
            vec![
                SessionEntryKind::OperationStarted {
                    operation_id: operation.clone(),
                },
                SessionEntryKind::TurnStarted {
                    operation_id: operation.clone(),
                    turn_id: turn.clone(),
                },
                SessionEntryKind::UserMessage {
                    turn_id: turn.clone(),
                    parts: philo_session::SessionUserPart::text_parts("broken round"),
                },
            ],
        ))
        .await
        .expect("seed turn start");
    let batch = sessions
        .commit(SessionTransaction::linear(
            session_id.clone(),
            start.revision(),
            vec![SessionEntryKind::AssistantToolCallBatch {
                turn_id: turn.clone(),
                model_call_id: "seed-call".to_owned(),
                tool_batch_id: philo_session::ToolBatchId::new("seed-batch"),
                blocks: vec![SessionAssistantBlock::ToolCall(SessionToolCall::new(
                    philo_session::ToolCallId::new("call-x"),
                    "read",
                    "this is not a json object",
                ))],
            }],
        ))
        .await
        .expect("seed tool batch");
    let results = sessions
        .commit(SessionTransaction::linear(
            session_id.clone(),
            batch.revision(),
            vec![SessionEntryKind::ToolResult {
                turn_id: turn.clone(),
                tool_batch_id: philo_session::ToolBatchId::new("seed-batch"),
                result: SessionToolResult::error(
                    philo_session::ToolCallId::new("call-x"),
                    "invalid_arguments",
                    "arguments must be a JSON object",
                ),
            }],
        ))
        .await
        .expect("seed tool result");
    sessions
        .commit(SessionTransaction::linear(
            session_id.clone(),
            results.revision(),
            vec![
                SessionEntryKind::AssistantMessage {
                    turn_id: turn.clone(),
                    blocks: vec![SessionAssistantBlock::Text {
                        text: "sorry, that failed".to_owned(),
                    }],
                },
                SessionEntryKind::TurnTerminated {
                    turn_id: turn.clone(),
                    outcome: TurnOutcome::Succeeded,
                },
                SessionEntryKind::OperationSettled {
                    operation_id: operation,
                    outcome: SessionOperationOutcome::Succeeded,
                },
            ],
        ))
        .await
        .expect("seed settlement");

    let transport =
        StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub-gpt", &["fine"]))]);
    let generation = compose_generation(transport.clone(), &root, 2);
    let parts = AgentRuntime::start(RuntimeDeps {
        sessions,
        ids: Arc::new(SequentialIdSource::new()),
        bounds: ChannelBounds::default(),
    })
    .expect("start runtime");
    let handle = parts.handle;
    let mut sub = parts.events;
    let accepted = handle
        .submit(OperationSpec {
            session_id: SessionId::new("m4-004"),
            user_message: UserMessage::new("try again"),
            generation,
            service_request_id: None,
        })
        .await
        .expect("submit accepted");
    let (_events, outcome) = drain_until_settled(&mut sub, &accepted.operation_id).await;
    assert!(matches!(
        outcome,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "fine"
    ));

    // The degraded call was replayed as `{}` with the paired error text.
    let body = &transport.request_bodies()[0];
    let assistant = &body["messages"][2];
    assert_eq!(assistant["tool_calls"][0]["id"], "call-x");
    assert_eq!(assistant["tool_calls"][0]["function"]["arguments"], "{}");
    assert_eq!(
        body["messages"][3]["content"][0]["text"],
        "invalid_arguments: arguments must be a JSON object"
    );
}

/// M4-005: `tools_allowed = false` maps to an empty tools list with
/// `ToolChoice::None` (the wire omits both fields).
#[tokio::test(flavor = "multi_thread")]
async fn zero_tool_rounds_requests_without_tools() {
    let root = TempRoot::new();
    root.file("hello.txt", "unused");
    let transport = StubTransport::new([StubResponse::Sse(text_sse(
        "resp-1",
        "stub-gpt",
        &["direct"],
    ))]);
    let sessions = Arc::new(MemorySessionStore::new());
    let generation = compose_generation(transport.clone(), &root, 0);
    let parts = AgentRuntime::start(RuntimeDeps {
        sessions,
        ids: Arc::new(SequentialIdSource::new()),
        bounds: ChannelBounds::default(),
    })
    .expect("start runtime");
    let handle = parts.handle;
    let mut sub = parts.events;

    let accepted = handle
        .submit(OperationSpec {
            session_id: SessionId::new("m4-005"),
            user_message: UserMessage::new("no tools"),
            generation,
            service_request_id: None,
        })
        .await
        .expect("submit accepted");
    let (_events, outcome) = drain_until_settled(&mut sub, &accepted.operation_id).await;
    assert!(matches!(
        outcome,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "direct"
    ));

    let body = &transport.request_bodies()[0];
    assert!(body.get("tools").is_none(), "tools must not be exposed");
    assert!(body.get("tool_choice").is_none());
}

/// M4-006: read-tool business errors (missing file, escaping path) become
/// durable `ToolResult::Error` facts and the loop continues to a final answer.
#[tokio::test(flavor = "multi_thread")]
async fn tool_business_errors_continue_the_loop() {
    let root = TempRoot::new();
    let transport = StubTransport::new([
        StubResponse::Sse(tool_round_sse("resp-1", "call-1", "missing.txt")),
        StubResponse::Sse(tool_round_sse("resp-2", "call-2", "../escape.txt")),
        StubResponse::Sse(text_sse("resp-3", "stub-gpt", &["both reads failed"])),
    ]);
    let sessions = Arc::new(MemorySessionStore::new());
    let generation = compose_generation(transport.clone(), &root, 2);
    let parts = AgentRuntime::start(RuntimeDeps {
        sessions: sessions.clone(),
        ids: Arc::new(SequentialIdSource::new()),
        bounds: ChannelBounds::default(),
    })
    .expect("start runtime");
    let handle = parts.handle;
    let mut sub = parts.events;

    let accepted = handle
        .submit(OperationSpec {
            session_id: SessionId::new("m4-006"),
            user_message: UserMessage::new("read stuff"),
            generation,
            service_request_id: None,
        })
        .await
        .expect("submit accepted");
    let (_events, outcome) = drain_until_settled(&mut sub, &accepted.operation_id).await;
    assert!(matches!(
        outcome,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "both reads failed"
    ));

    // Both business errors are durable facts with stable codes.
    let view = sessions
        .context_view(&philo_session::SessionId::new("m4-006"))
        .await
        .expect("context view");
    let error_codes: Vec<String> = view
        .messages()
        .iter()
        .filter_map(|message| match message {
            ContextMessage::ToolResult {
                outcome: ToolResultOutcome::Error { code, .. },
                ..
            } => Some(code.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(error_codes, vec!["not_found", "outside_root"]);

    // The second and third calls saw the prior error text in their transcript.
    let bodies = transport.request_bodies();
    assert_eq!(bodies.len(), 3);
    let second = &bodies[1];
    let error_text = second["messages"][3]["content"][0]["text"]
        .as_str()
        .expect("first error text");
    assert!(error_text.starts_with("not_found:"));
    let third = &bodies[2];
    let second_error_text = third["messages"][5]["content"][0]["text"]
        .as_str()
        .expect("second error text");
    assert!(second_error_text.starts_with("outside_root:"));
}
