//! MODEL-003: reasoning normalization, the same-turn replay side channel,
//! reasoning-effort mapping, and usage forwarding (M7-001 ~ M7-005).
//!
//! Reasoning coverage targets the OpenAI-compatible baseline: the
//! `reasoning_content` chat dialect streams visible reasoning, and the
//! official OpenAI shape carries the effort control. Protocols with signed
//! or opaque reasoning (Anthropic thinking, OpenAI Responses) follow later
//! milestones as the SDK evolves.

mod support;

use philo_agent_runtime::{
    ModelEvent, ModelMessage, ModelPort, ModelToolCall, ModelToolResultOutcome, ReasoningEffort,
    ToolCallId, UserPart,
};
use philo_model::{ModelProtocol, PhiloModelAdapter};
use support::{
    StubResponse, StubTransport, adapter_over, collect_ok, read_tool_definition,
    reasoning_snapshot, snapshot, sse, text_sse,
};

fn user(content: &str) -> ModelMessage {
    ModelMessage::User {
        parts: vec![UserPart::Text(content.to_owned())],
    }
}

fn reasoning_content_adapter(transport: StubTransport) -> PhiloModelAdapter {
    PhiloModelAdapter::builder(
        "stub-provider",
        ModelProtocol::OpenAiChatReasoningContent,
        "stub-model",
        support::STUB_ENDPOINT,
    )
    .build_with_transport(transport)
    .expect("reasoning-content adapter assembly")
}

/// Call one: two visible reasoning deltas, then one `read` tool call.
fn reasoning_tool_call_body() -> Vec<u8> {
    let head = r#"{"id":"resp-1","object":"chat.completion.chunk","model":"stub-r1","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"think "},"finish_reason":null}]}"#;
    let more_reasoning =
        r#"{"choices":[{"index":0,"delta":{"reasoning_content":"hard"},"finish_reason":null}]}"#;
    let tool_call = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"read","arguments":"{\"path\":\"a.txt\"}"}}]},"finish_reason":null}]}"#;
    let finish = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;
    sse(&[head, more_reasoning, tool_call, finish, "[DONE]"])
}

/// The turn transcript as the runtime replays it for the second call.
fn second_call_messages() -> Vec<ModelMessage> {
    vec![
        user("go"),
        ModelMessage::AssistantToolCalls {
            calls: vec![ModelToolCall {
                tool_call_id: ToolCallId::new("call-1"),
                name: "read".to_owned(),
                arguments: r#"{"path":"a.txt"}"#.to_owned(),
            }],
        },
        ModelMessage::ToolResult {
            tool_call_id: ToolCallId::new("call-1"),
            outcome: ModelToolResultOutcome::Success {
                content: "alpha".to_owned(),
            },
        },
    ]
}

// --- M7-001: transient reasoning events ---------------------------------------

#[tokio::test]
async fn reasoning_stream_normalizes_to_transient_reasoning_events() {
    let transport = StubTransport::new([StubResponse::Sse(reasoning_tool_call_body())]);
    let adapter = reasoning_content_adapter(transport);
    let stream = adapter
        .start(reasoning_snapshot(
            "turn-1",
            1,
            None,
            vec![user("go")],
            vec![read_tool_definition()],
        ))
        .await
        .expect("call starts");
    let events = collect_ok(stream).await;

    let reasoning: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            ModelEvent::ReasoningDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, ["think ", "hard"], "visible deltas stream out");

    // Reasoning never joins the assembled output vocabulary: the tool call
    // and terminal events keep their existing shapes and order.
    let tool_delta = events
        .iter()
        .position(|event| matches!(event, ModelEvent::ToolCallDelta { .. }))
        .expect("tool call surfaces");
    let last_reasoning = events
        .iter()
        .rposition(|event| matches!(event, ModelEvent::ReasoningDelta { .. }))
        .unwrap();
    assert!(
        last_reasoning < tool_delta,
        "reasoning precedes the tool call"
    );
    assert!(matches!(events.last(), Some(ModelEvent::Completed)));
}

// --- M7-002: same-turn replay and cross-turn isolation --------------------------

#[tokio::test]
async fn same_turn_second_call_replays_the_reasoning_state() {
    let transport = StubTransport::new([
        StubResponse::Sse(reasoning_tool_call_body()),
        StubResponse::Sse(text_sse("resp-2", "stub-r1", &["done"])),
    ]);
    let adapter = reasoning_content_adapter(transport.clone());

    let first = adapter
        .start(reasoning_snapshot(
            "turn-1",
            1,
            None,
            vec![user("go")],
            vec![read_tool_definition()],
        ))
        .await
        .expect("first call starts");
    collect_ok(first).await;

    let second = adapter
        .start(reasoning_snapshot(
            "turn-1",
            2,
            None,
            second_call_messages(),
            vec![read_tool_definition()],
        ))
        .await
        .expect("second call starts");
    collect_ok(second).await;

    let bodies = transport.request_bodies();
    assert_eq!(bodies.len(), 2);

    // The first request carries no reasoning history.
    let first_assistants: Vec<_> = bodies[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "assistant")
        .collect();
    assert!(first_assistants.is_empty());

    // The second request replays the captured reasoning on the assistant
    // tool-call message of the same turn.
    let assistant = &bodies[1]["messages"][1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["reasoning_content"], "think hard");
    assert_eq!(assistant["tool_calls"][0]["id"], "call-1");
}

#[tokio::test]
async fn a_new_turn_first_call_never_replays_stale_state() {
    let transport = StubTransport::new([
        StubResponse::Sse(reasoning_tool_call_body()),
        StubResponse::Sse(text_sse("resp-2", "stub-r1", &["next answer"])),
    ]);
    let adapter = reasoning_content_adapter(transport.clone());

    // Turn 1 call 1 caches reasoning state...
    let first = adapter
        .start(reasoning_snapshot(
            "turn-1",
            1,
            None,
            vec![user("go")],
            vec![read_tool_definition()],
        ))
        .await
        .expect("first call starts");
    collect_ok(first).await;

    // ...but turn 2's first call replays nothing, even though the prior
    // turn's tool exchange is part of its context.
    let mut next_turn_messages = second_call_messages();
    next_turn_messages.push(user("next question"));
    let second = adapter
        .start(reasoning_snapshot(
            "turn-2",
            1,
            None,
            next_turn_messages,
            vec![read_tool_definition()],
        ))
        .await
        .expect("new turn starts");
    collect_ok(second).await;

    let bodies = transport.request_bodies();
    let assistant = &bodies[1]["messages"][1];
    assert_eq!(assistant["role"], "assistant");
    assert!(
        assistant.get("reasoning_content").is_none(),
        "no stale reasoning crosses the turn boundary"
    );
    assert_eq!(assistant["tool_calls"][0]["id"], "call-1");
}

#[tokio::test]
async fn a_cancelled_turn_leaves_no_reasoning_for_the_next_turn() {
    // The first call's stream is dropped mid-flight (the runtime's
    // cancellation signal): nothing may be committed to the side channel.
    let head = r#"{"id":"resp-1","object":"chat.completion.chunk","model":"stub-r1","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"secret"},"finish_reason":null}]}"#;
    let transport = StubTransport::new([
        StubResponse::Sse(sse(&[head])),
        StubResponse::Sse(text_sse("resp-2", "stub-r1", &["fresh"])),
    ]);
    let adapter = reasoning_content_adapter(transport.clone());

    let mut first = adapter
        .start(reasoning_snapshot(
            "turn-1",
            1,
            None,
            vec![user("go")],
            vec![read_tool_definition()],
        ))
        .await
        .expect("first call starts");
    let _ = first.next().await;
    drop(first);

    // Even a later call of the SAME turn sees no committed reasoning: the
    // side channel commits only at normal stream completion.
    let second = adapter
        .start(reasoning_snapshot(
            "turn-1",
            2,
            None,
            second_call_messages(),
            vec![read_tool_definition()],
        ))
        .await
        .expect("second call starts");
    collect_ok(second).await;

    let bodies = transport.request_bodies();
    let assistant = &bodies[1]["messages"][1];
    assert!(
        assistant.get("reasoning_content").is_none(),
        "a dropped stream never commits reasoning state"
    );
}

// --- M7-004: reasoning-effort mapping and negotiation failure --------------------

#[tokio::test]
async fn reasoning_effort_maps_into_the_official_openai_request() {
    let transport =
        StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub-gpt", &["ok"]))]);
    let adapter = adapter_over(transport.clone());
    let stream = adapter
        .start(reasoning_snapshot(
            "turn-1",
            1,
            Some(ReasoningEffort::Low),
            vec![user("hi")],
            Vec::new(),
        ))
        .await
        .expect("call starts");
    collect_ok(stream).await;
    assert_eq!(transport.request_bodies()[0]["reasoning_effort"], "low");
}

#[tokio::test]
async fn disabled_reasoning_keeps_the_baseline_request_shape() {
    let transport =
        StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub-gpt", &["ok"]))]);
    let adapter = adapter_over(transport.clone());
    let stream = adapter
        .start(snapshot(vec![user("hi")], Vec::new()))
        .await
        .expect("call starts");
    collect_ok(stream).await;
    assert!(
        transport.request_bodies()[0]
            .get("reasoning_effort")
            .is_none(),
        "None keeps the pre-M7 request shape"
    );
}

#[tokio::test]
async fn unsupported_reasoning_effort_is_a_configuration_model_error() {
    let transport = StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub", &["never"]))]);
    // The conservative compatible profile has no reasoning control at all.
    let adapter = PhiloModelAdapter::builder(
        "stub-provider",
        ModelProtocol::OpenAiChatCompatible,
        "stub-model",
        support::STUB_ENDPOINT,
    )
    .build_with_transport(transport.clone())
    .expect("compatible adapter assembly");

    let error = adapter
        .start(reasoning_snapshot(
            "turn-1",
            1,
            Some(ReasoningEffort::High),
            vec![user("hi")],
            Vec::new(),
        ))
        .await
        .err()
        .expect("capability negotiation must reject the effort");
    assert!(
        error.message().contains("Capability"),
        "configuration-class error: {}",
        error.message()
    );
    assert!(
        transport.requests().is_empty(),
        "rejected before any transport call"
    );
}

// --- M7-005: usage forwarding ------------------------------------------------------

#[tokio::test]
async fn reasoning_dialect_usage_maps_onto_token_usage() {
    let head = r#"{"id":"resp-1","object":"chat.completion.chunk","model":"stub-r1","choices":[{"index":0,"delta":{"role":"assistant","content":"ok"},"finish_reason":null}]}"#;
    let finish = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":4,"completion_tokens_details":{"reasoning_tokens":2}}}"#;
    let transport = StubTransport::new([StubResponse::Sse(sse(&[head, finish, "[DONE]"]))]);
    let adapter = reasoning_content_adapter(transport);
    let stream = adapter
        .start(reasoning_snapshot(
            "turn-1",
            1,
            None,
            vec![user("hi")],
            Vec::new(),
        ))
        .await
        .expect("call starts");
    let events = collect_ok(stream).await;
    let usages: Vec<philo_agent_runtime::TokenUsage> = events
        .iter()
        .filter_map(|event| match event {
            ModelEvent::UsageUpdated { usage } => Some(*usage),
            _ => None,
        })
        .collect();
    assert_eq!(usages.len(), 1);
    assert_eq!(usages[0].input_tokens, Some(11));
    assert_eq!(usages[0].output_tokens, Some(4));
    assert_eq!(usages[0].total_tokens(), Some(15));
}
