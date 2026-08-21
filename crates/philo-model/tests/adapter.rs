//! MODEL-001: philo-model adapter focused tests over a scripted stub transport.
//!
//! Covers the model-adapter delta's request-mapping and event-normalization
//! tables row by row, plus error normalization. Everything here runs offline.

mod support;

use std::time::Duration;

use http::header::{AUTHORIZATION, USER_AGENT};
use philo_agent_runtime::{
    ModelAssistantBlock, ModelEvent, ModelMessage, ModelPort, ModelToolCall,
    ModelToolResultOutcome, ToolCallId, UserPart,
};
use philo_model::{
    DEFAULT_USER_AGENT, ModelCompat, ModelProtocol, ModelRequestHeaders, PhiloModelAdapter,
    TimeoutPolicy,
};
use support::{
    StubResponse, StubTransport, adapter_over, assistant_text, assistant_tool_calls, collect,
    collect_ok, completed_text, read_tool_definition, snapshot, sse, text_sse,
};

fn user(content: &str) -> ModelMessage {
    ModelMessage::User {
        parts: vec![UserPart::Text(content.to_owned())],
    }
}

fn system(content: &str) -> ModelMessage {
    ModelMessage::System {
        content: content.to_owned(),
    }
}

// --- Request mapping -------------------------------------------------------

#[tokio::test]
async fn request_maps_system_history_tools_and_generation() {
    let transport =
        StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub-gpt", &["ok"]))]);
    let adapter = adapter_over(transport.clone());
    let stream = adapter
        .start(snapshot(
            vec![
                system("be helpful"),
                user("hello"),
                assistant_text("earlier answer"),
                user("again"),
            ],
            vec![read_tool_definition()],
        ))
        .await
        .expect("call starts");
    collect_ok(stream).await;

    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, http::Method::POST);
    assert_eq!(requests[0].url.as_str(), support::STUB_ENDPOINT);

    let body = &transport.request_bodies()[0];
    assert_eq!(body["model"], "stub-model");
    assert_eq!(body["stream"], true);
    // openai-chat/openai/v1 renders instructions as a developer message.
    assert_eq!(body["messages"][0]["role"], "developer");
    assert_eq!(body["messages"][0]["content"], "be helpful");
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["messages"][1]["content"][0]["text"], "hello");
    assert_eq!(body["messages"][2]["role"], "assistant");
    assert_eq!(body["messages"][2]["content"][0]["text"], "earlier answer");
    assert_eq!(body["messages"][3]["role"], "user");
    assert_eq!(body["messages"][3]["content"][0]["text"], "again");
    // GenerationConfig: u32 limit passes through, f32 temperature widens to f64.
    assert_eq!(body["max_completion_tokens"], 256);
    assert_eq!(body["temperature"], 0.25);
    assert_eq!(body["prompt_cache_key"], "session-1");
    // Frozen tool definitions pass in order with ToolChoice::Auto.
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "read");
    assert_eq!(body["tools"][0]["function"]["description"], "Read a file");
    assert_eq!(
        body["tools"][0]["function"]["parameters"]["required"][0],
        "path"
    );
    assert_eq!(body["tool_choice"], "auto");
}

/// ADR-0006 accepts an empty block list as "empty final text"; the SDK still
/// requires non-empty assistant content, so replay maps it to the `(empty)`
/// marker instead of failing request validation at Prepare.
#[tokio::test]
async fn empty_assistant_history_replays_as_placeholder_text() {
    let transport =
        StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub-gpt", &["ok"]))]);
    let adapter = adapter_over(transport.clone());
    let stream = adapter
        .start(snapshot(
            vec![
                user("hello"),
                ModelMessage::Assistant { blocks: Vec::new() },
                user("continue"),
            ],
            Vec::new(),
        ))
        .await
        .expect("empty assistant history must not fail request validation");
    collect_ok(stream).await;

    let body = &transport.request_bodies()[0];
    assert_eq!(body["messages"][1]["role"], "assistant");
    assert_eq!(body["messages"][1]["content"][0]["text"], "(empty)");
    assert_eq!(body["messages"][2]["role"], "user");
}

#[tokio::test]
async fn compatible_chat_sends_cache_identity_and_affinity_headers() {
    let transport =
        StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub-gpt", &["ok"]))]);
    let adapter = PhiloModelAdapter::builder(
        "stub-provider",
        ModelProtocol::OpenAiChat,
        "stub-model",
        support::STUB_ENDPOINT,
    )
    .compat(ModelCompat::Compatible)
    .build_with_transport(transport.clone())
    .expect("compatible adapter assembly");
    let stream = adapter
        .start(snapshot(vec![user("hello")], Vec::new()))
        .await
        .expect("call starts");
    collect_ok(stream).await;

    let body = &transport.request_bodies()[0];
    assert_eq!(body["prompt_cache_key"], "session-1");
    assert_eq!(body["stream_options"]["include_usage"], true);
    let headers = &transport.requests()[0].headers;
    assert_eq!(headers["session_id"], "session-1");
    assert_eq!(headers["x-client-request-id"], "session-1");
    assert_eq!(headers["x-session-affinity"], "session-1");
}

const STUB_RESPONSES_ENDPOINT: &str = "https://stub.invalid/v1/responses";
const MINIMAL_RESPONSE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../philo/crates/philo/tests/fixtures/openai_responses/stream/minimal.sse"
));
const COMPAT_MINIMAL_RESPONSE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../philo/crates/philo/tests/fixtures/openai_responses/stream/compat-minimal.sse"
));

#[tokio::test]
async fn official_responses_sends_prompt_cache_key() {
    let transport = StubTransport::new([StubResponse::Sse(MINIMAL_RESPONSE.to_vec())]);
    let adapter = PhiloModelAdapter::builder(
        "stub-provider",
        ModelProtocol::OpenAiResponses,
        "stub-model",
        STUB_RESPONSES_ENDPOINT,
    )
    .compat(ModelCompat::Official)
    .build_with_transport(transport.clone())
    .expect("official Responses adapter assembly");
    let stream = adapter
        .start(snapshot(vec![user("hello")], Vec::new()))
        .await
        .expect("call starts");
    collect_ok(stream).await;

    let body = &transport.request_bodies()[0];
    assert_eq!(body["prompt_cache_key"], "session-1");
    let headers = &transport.requests()[0].headers;
    assert!(headers.get("session_id").is_none());
    assert!(headers.get("x-client-request-id").is_none());
    assert!(headers.get("x-session-affinity").is_none());
}

#[tokio::test]
async fn compatible_responses_sends_cache_identity_and_affinity_headers() {
    let transport = StubTransport::new([StubResponse::Sse(COMPAT_MINIMAL_RESPONSE.to_vec())]);
    let adapter = PhiloModelAdapter::builder(
        "stub-provider",
        ModelProtocol::OpenAiResponses,
        "stub-model",
        STUB_RESPONSES_ENDPOINT,
    )
    .compat(ModelCompat::Compatible)
    .build_with_transport(transport.clone())
    .expect("compatible Responses adapter assembly");
    let stream = adapter
        .start(snapshot(vec![user("hello")], Vec::new()))
        .await
        .expect("call starts");
    collect_ok(stream).await;

    let body = &transport.request_bodies()[0];
    assert_eq!(body["prompt_cache_key"], "session-1");
    let headers = &transport.requests()[0].headers;
    assert_eq!(headers["session_id"], "session-1");
    assert_eq!(headers["x-client-request-id"], "session-1");
    assert_eq!(headers["x-session-affinity"], "session-1");
}

#[tokio::test]
async fn request_maps_tool_transcript_messages() {
    let transport =
        StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub-gpt", &["ok"]))]);
    let adapter = adapter_over(transport.clone());
    let stream = adapter
        .start(snapshot(
            vec![
                user("read two files"),
                assistant_tool_calls([
                    ModelToolCall {
                        tool_call_id: ToolCallId::new("call-a"),
                        name: "read".to_owned(),
                        arguments: r#"{"path":"a.txt"}"#.to_owned(),
                    },
                    ModelToolCall {
                        tool_call_id: ToolCallId::new("call-b"),
                        name: "read".to_owned(),
                        arguments: r#"{"path":"b.txt"}"#.to_owned(),
                    },
                ]),
                ModelMessage::ToolResult {
                    tool_call_id: ToolCallId::new("call-a"),
                    outcome: ModelToolResultOutcome::Success {
                        content: "alpha".to_owned(),
                    },
                },
                ModelMessage::ToolResult {
                    tool_call_id: ToolCallId::new("call-b"),
                    outcome: ModelToolResultOutcome::Error {
                        code: "not_found".to_owned(),
                        message: "file not found: b.txt".to_owned(),
                    },
                },
            ],
            vec![read_tool_definition()],
        ))
        .await
        .expect("call starts");
    collect_ok(stream).await;

    let body = &transport.request_bodies()[0];
    // No system prompt in the snapshot: the first message is the user turn.
    assert_eq!(body["messages"][0]["role"], "user");
    let assistant = &body["messages"][1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["tool_calls"][0]["id"], "call-a");
    assert_eq!(assistant["tool_calls"][0]["type"], "function");
    assert_eq!(assistant["tool_calls"][0]["function"]["name"], "read");
    assert_eq!(
        assistant["tool_calls"][0]["function"]["arguments"],
        r#"{"path":"a.txt"}"#
    );
    assert_eq!(assistant["tool_calls"][1]["id"], "call-b");

    let success = &body["messages"][2];
    assert_eq!(success["role"], "tool");
    assert_eq!(success["tool_call_id"], "call-a");
    assert_eq!(success["content"][0]["text"], "alpha");

    // Error results carry a non-empty `code: message` text block. OpenAI Chat
    // has no native error status, so the text travels as plain result content.
    let error = &body["messages"][3];
    assert_eq!(error["role"], "tool");
    assert_eq!(error["tool_call_id"], "call-b");
    assert_eq!(
        error["content"][0]["text"],
        "not_found: file not found: b.txt"
    );
}

#[tokio::test]
async fn empty_tool_success_text_maps_to_placeholder() {
    let transport =
        StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub-gpt", &["ok"]))]);
    let adapter = adapter_over(transport.clone());
    let stream = adapter
        .start(snapshot(
            vec![
                user("read the empty file"),
                assistant_tool_calls([ModelToolCall {
                    tool_call_id: ToolCallId::new("call-1"),
                    name: "read".to_owned(),
                    arguments: "{}".to_owned(),
                }]),
                ModelMessage::ToolResult {
                    tool_call_id: ToolCallId::new("call-1"),
                    outcome: ModelToolResultOutcome::Success {
                        content: String::new(),
                    },
                },
            ],
            vec![read_tool_definition()],
        ))
        .await
        .expect("call starts");
    collect_ok(stream).await;

    let body = &transport.request_bodies()[0];
    assert_eq!(body["messages"][2]["content"][0]["text"], "(empty)");
}

#[tokio::test]
async fn disabled_tools_map_to_empty_tools_and_choice_none() {
    let transport =
        StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub-gpt", &["ok"]))]);
    let adapter = adapter_over(transport.clone());
    let stream = adapter
        .start(snapshot(vec![system("sys"), user("hello")], Vec::new()))
        .await
        .expect("call starts");
    collect_ok(stream).await;

    // The wire omits both fields entirely for empty tools + ToolChoice::None.
    let body = &transport.request_bodies()[0];
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
}

#[tokio::test]
async fn invalid_raw_arguments_replay_degraded_to_empty_object() {
    let transport =
        StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub-gpt", &["ok"]))]);
    let adapter = adapter_over(transport.clone());
    let stream = adapter
        .start(snapshot(
            vec![
                user("go"),
                assistant_tool_calls([ModelToolCall {
                    tool_call_id: ToolCallId::new("call-1"),
                    name: "read".to_owned(),
                    arguments: "this is not json".to_owned(),
                }]),
                ModelMessage::ToolResult {
                    tool_call_id: ToolCallId::new("call-1"),
                    outcome: ModelToolResultOutcome::Error {
                        code: "invalid_arguments".to_owned(),
                        message: "arguments must be a JSON object".to_owned(),
                    },
                },
            ],
            vec![read_tool_definition()],
        ))
        .await
        .expect("degraded replay still starts");
    collect_ok(stream).await;

    let body = &transport.request_bodies()[0];
    assert_eq!(
        body["messages"][1]["tool_calls"][0]["function"]["arguments"], "{}",
        "invalid raw arguments replay as an empty object"
    );
    // The paired error text keeps the full details for the model.
    assert_eq!(
        body["messages"][2]["content"][0]["text"],
        "invalid_arguments: arguments must be a JSON object"
    );
}

#[tokio::test]
async fn zero_max_output_tokens_fails_before_any_transport_call() {
    let transport = StubTransport::new([]);
    let adapter = adapter_over(transport.clone());
    let mut invalid = snapshot(vec![user("hello")], Vec::new());
    invalid.generation.max_output_tokens = 0;
    let error = adapter.start(invalid).await.err().expect("must fail");
    assert!(
        error.message().contains("max_output_tokens"),
        "unexpected message: {}",
        error.message()
    );
    assert!(transport.requests().is_empty(), "no request may be sent");
}

// --- Event normalization ----------------------------------------------------

#[tokio::test]
async fn text_stream_normalizes_started_deltas_and_unique_completed() {
    let transport = StubTransport::new([StubResponse::Sse(text_sse(
        "resp-42",
        "stub-gpt-mini",
        &["Hel", "lo"],
    ))]);
    let adapter = adapter_over(transport);
    let stream = adapter
        .start(snapshot(vec![user("hi")], Vec::new()))
        .await
        .expect("call starts");
    let events = collect_ok(stream).await;
    assert_eq!(
        events,
        vec![
            ModelEvent::ResponseStarted {
                response_model: Some("stub-gpt-mini".to_owned()),
                response_id: Some("resp-42".to_owned()),
            },
            ModelEvent::TextDelta("Hel".to_owned()),
            ModelEvent::TextDelta("lo".to_owned()),
            completed_text("Hello"),
        ]
    );
}

#[tokio::test]
async fn tool_call_stream_aggregates_interleaved_blocks_in_source_order() {
    let head = r#"{"id":"resp-9","object":"chat.completion.chunk","model":"stub-gpt","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#;
    let start_a = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-a","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#;
    let start_b = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call-b","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#;
    let args_a1 = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"a.txt\""}}]},"finish_reason":null}]}"#;
    let args_b = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"path\":\"b.txt\"}"}}]},"finish_reason":null}]}"#;
    let args_a2 = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"}"}}]},"finish_reason":null}]}"#;
    let finish = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;
    let transport = StubTransport::new([StubResponse::Sse(sse(&[
        head, start_a, start_b, args_a1, args_b, args_a2, finish, "[DONE]",
    ]))]);
    let adapter = adapter_over(transport);
    let stream = adapter
        .start(snapshot(
            vec![user("read both")],
            vec![read_tool_definition()],
        ))
        .await
        .expect("call starts");
    let events = collect_ok(stream).await;
    assert_eq!(
        events,
        vec![
            ModelEvent::ResponseStarted {
                response_model: Some("stub-gpt".to_owned()),
                response_id: Some("resp-9".to_owned()),
            },
            // Stable batch index in first-appearance order; the stable id and
            // name surface exactly once per call.
            ModelEvent::ToolCallDelta {
                index: 0,
                id: Some("call-a".to_owned()),
                name: Some("read".to_owned()),
                arguments: String::new(),
            },
            ModelEvent::ToolCallDelta {
                index: 1,
                id: Some("call-b".to_owned()),
                name: Some("read".to_owned()),
                arguments: String::new(),
            },
            ModelEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments: "{\"path\":\"a.txt\"".to_owned(),
            },
            ModelEvent::ToolCallDelta {
                index: 1,
                id: None,
                name: None,
                arguments: "{\"path\":\"b.txt\"}".to_owned(),
            },
            ModelEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments: "}".to_owned(),
            },
            ModelEvent::Completed {
                blocks: vec![
                    ModelAssistantBlock::ToolCall(ModelToolCall {
                        tool_call_id: ToolCallId::new("call-a"),
                        name: "read".to_owned(),
                        arguments: r#"{"path":"a.txt"}"#.to_owned(),
                    }),
                    ModelAssistantBlock::ToolCall(ModelToolCall {
                        tool_call_id: ToolCallId::new("call-b"),
                        name: "read".to_owned(),
                        arguments: r#"{"path":"b.txt"}"#.to_owned(),
                    }),
                ],
            },
        ]
    );
}

#[tokio::test]
async fn mixed_text_and_tool_completes_as_ordered_blocks_and_replays_as_one_assistant() {
    let head = r#"{"id":"resp-mix","object":"chat.completion.chunk","model":"stub-gpt","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#;
    let text =
        r#"{"choices":[{"index":0,"delta":{"content":"Let me look"},"finish_reason":null}]}"#;
    let start = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#;
    let args = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"a.txt\"}"}}]},"finish_reason":null}]}"#;
    let finish = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;
    let transport = StubTransport::new([
        StubResponse::Sse(sse(&[head, text, start, args, finish, "[DONE]"])),
        StubResponse::Sse(text_sse("resp-2", "stub-gpt", &["done"])),
    ]);
    let adapter = adapter_over(transport.clone());
    let events = collect_ok(
        adapter
            .start(snapshot(
                vec![user("look this up")],
                vec![read_tool_definition()],
            ))
            .await
            .expect("mixed call starts"),
    )
    .await;
    let Some(ModelEvent::Completed { blocks }) = events.last() else {
        panic!("expected Completed with blocks, got {:?}", events.last());
    };
    assert_eq!(
        blocks,
        &vec![
            ModelAssistantBlock::Text {
                text: "Let me look".to_owned(),
            },
            ModelAssistantBlock::ToolCall(ModelToolCall {
                tool_call_id: ToolCallId::new("call-1"),
                name: "read".to_owned(),
                arguments: r#"{"path":"a.txt"}"#.to_owned(),
            }),
        ]
    );

    collect_ok(
        adapter
            .start(snapshot(
                vec![
                    user("look this up"),
                    ModelMessage::Assistant {
                        blocks: blocks.clone(),
                    },
                    ModelMessage::ToolResult {
                        tool_call_id: ToolCallId::new("call-1"),
                        outcome: ModelToolResultOutcome::Success {
                            content: "alpha".to_owned(),
                        },
                    },
                ],
                vec![read_tool_definition()],
            ))
            .await
            .expect("replay call starts"),
    )
    .await;

    let body = &transport.request_bodies()[1];
    let assistants: Vec<&serde_json::Value> = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|message| message["role"] == "assistant")
        .collect();
    assert_eq!(
        assistants.len(),
        1,
        "text and tool stay one assistant message: {assistants:?}"
    );
    assert_eq!(assistants[0]["content"][0]["text"], "Let me look");
    assert_eq!(assistants[0]["tool_calls"][0]["id"], "call-1");
    assert_eq!(assistants[0]["tool_calls"][0]["function"]["name"], "read");
    assert_eq!(
        assistants[0]["tool_calls"][0]["function"]["arguments"],
        r#"{"path":"a.txt"}"#
    );
}

/// M7 updated this pin: usage observations now map onto the runtime
/// `TokenUsage` vocabulary instead of being dropped (M4 behavior).
#[tokio::test]
async fn usage_updates_are_forwarded_in_the_normalized_stream() {
    let head = r#"{"id":"resp-1","object":"chat.completion.chunk","model":"stub-gpt","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#;
    let content = r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]}"#;
    let finish_with_usage = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2}}"#;
    let transport = StubTransport::new([StubResponse::Sse(sse(&[
        head,
        content,
        finish_with_usage,
        "[DONE]",
    ]))]);
    let adapter = adapter_over(transport);
    let stream = adapter
        .start(snapshot(vec![user("hi")], Vec::new()))
        .await
        .expect("call starts");
    let events = collect_ok(stream).await;
    assert_eq!(
        events,
        vec![
            ModelEvent::ResponseStarted {
                response_model: Some("stub-gpt".to_owned()),
                response_id: Some("resp-1".to_owned()),
            },
            ModelEvent::TextDelta("ok".to_owned()),
            ModelEvent::UsageUpdated {
                usage: philo_agent_runtime::TokenUsage {
                    input_tokens: Some(10),
                    output_tokens: Some(2),
                    ..philo_agent_runtime::TokenUsage::default()
                },
            },
            completed_text("ok"),
        ],
        "usage maps onto the runtime TokenUsage; block boundaries stay dropped"
    );
}

// --- Error normalization ----------------------------------------------------

#[tokio::test]
async fn mid_stream_decode_error_terminates_without_completed() {
    let head = r#"{"id":"resp-1","object":"chat.completion.chunk","model":"stub-gpt","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#;
    let content = r#"{"choices":[{"index":0,"delta":{"content":"Hel"},"finish_reason":null}]}"#;
    let transport = StubTransport::new([StubResponse::Sse(sse(&[head, content, "not-json"]))]);
    let adapter = adapter_over(transport);
    let stream = adapter
        .start(snapshot(vec![user("hi")], Vec::new()))
        .await
        .expect("call starts");
    let events = collect(stream).await;
    assert_eq!(events.len(), 3, "started, one delta, then the error");
    assert!(matches!(&events[0], Ok(ModelEvent::ResponseStarted { .. })));
    assert_eq!(events[1], Ok(ModelEvent::TextDelta("Hel".to_owned())));
    let error = events[2].as_ref().expect_err("terminal error");
    assert!(
        error.message().contains("ProtocolDecode"),
        "kind summary expected: {}",
        error.message()
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ModelEvent::Completed { .. }))),
        "no Completed after a mid-stream error"
    );
}

#[tokio::test]
async fn connect_failure_normalizes_to_a_transport_model_error() {
    let transport = StubTransport::new([StubResponse::ConnectError]);
    let adapter = adapter_over(transport);
    let error = adapter
        .start(snapshot(vec![user("hi")], Vec::new()))
        .await
        .err()
        .expect("connect failure surfaces at start");
    assert!(
        error.message().contains("Transport"),
        "kind summary expected: {}",
        error.message()
    );
}

#[tokio::test]
async fn provider_http_error_normalizes_to_a_model_error() {
    let transport = StubTransport::new([StubResponse::Status(
        http::StatusCode::INTERNAL_SERVER_ERROR,
        br#"{"error":{"message":"boom","type":"server_error"}}"#.to_vec(),
    )]);
    let adapter = adapter_over(transport);
    let error = adapter
        .start(snapshot(vec![user("hi")], Vec::new()))
        .await
        .err()
        .expect("http failure surfaces at start");
    assert!(
        error.message().contains("ProviderHttp"),
        "kind summary expected: {}",
        error.message()
    );
}

#[tokio::test(start_paused = true)]
async fn response_head_timeout_normalizes_to_a_timeout_model_error() {
    let transport = StubTransport::new([StubResponse::Hang]);
    let adapter = PhiloModelAdapter::builder(
        "stub-provider",
        ModelProtocol::OpenAiChat,
        "stub-model",
        support::STUB_ENDPOINT,
    )
    .timeout_policy(TimeoutPolicy {
        response_head: Some(Duration::from_millis(50)),
        ..TimeoutPolicy::default()
    })
    .build_with_transport(transport)
    .expect("adapter assembly");
    let error = adapter
        .start(snapshot(vec![user("hi")], Vec::new()))
        .await
        .err()
        .expect("timeout surfaces at start");
    assert!(
        error.message().contains("Timeout"),
        "kind summary expected: {}",
        error.message()
    );
}

// --- Assembly ---------------------------------------------------------------

#[tokio::test]
async fn api_key_env_resolves_to_a_bearer_authorization_header() {
    // SAFETY: test-local variable name; the test harness may run tests in
    // threads, so the name is unique to this test.
    unsafe { std::env::set_var("PHILO_MODEL_M4_TEST_KEY", "test-secret") };
    let transport =
        StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub-gpt", &["ok"]))]);
    let adapter = PhiloModelAdapter::builder(
        "stub-provider",
        ModelProtocol::OpenAiChat,
        "stub-model",
        support::STUB_ENDPOINT,
    )
    .api_key_env("PHILO_MODEL_M4_TEST_KEY")
    .build_with_transport(transport.clone())
    .expect("adapter assembly");
    let stream = adapter
        .start(snapshot(vec![user("hi")], Vec::new()))
        .await
        .expect("call starts");
    collect_ok(stream).await;

    let requests = transport.requests();
    assert_eq!(requests[0].headers[USER_AGENT], DEFAULT_USER_AGENT);
    let authorization = requests[0]
        .headers
        .get(AUTHORIZATION)
        .expect("authorization header present");
    assert_eq!(authorization.to_str().unwrap(), "Bearer test-secret");
}

#[tokio::test]
async fn deployment_headers_override_the_default_user_agent_and_reach_transport() {
    let transport =
        StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub-gpt", &["ok"]))]);
    let headers = ModelRequestHeaders::try_from_iter([
        ("User-Agent", "configured-agent/1"),
        ("X-Route", "deployment-a"),
    ])
    .expect("valid request headers");
    let adapter = PhiloModelAdapter::builder(
        "stub-provider",
        ModelProtocol::OpenAiChat,
        "stub-model",
        support::STUB_ENDPOINT,
    )
    .request_headers(headers)
    .build_with_transport(transport.clone())
    .expect("adapter assembly");

    let stream = adapter
        .start(snapshot(vec![user("hi")], Vec::new()))
        .await
        .expect("call starts");
    collect_ok(stream).await;

    let requests = transport.requests();
    assert_eq!(requests[0].headers[USER_AGENT], "configured-agent/1");
    assert_eq!(requests[0].headers["x-route"], "deployment-a");
}

#[tokio::test]
async fn invalid_endpoint_fails_assembly() {
    let error = PhiloModelAdapter::builder(
        "stub-provider",
        ModelProtocol::OpenAiChat,
        "stub-model",
        "not a url",
    )
    .build_with_transport(StubTransport::new([]))
    .err()
    .expect("assembly must fail");
    assert!(error.message().contains("endpoint"));
}
