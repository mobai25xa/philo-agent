//! MODEL-002: `Cancelled` outcome mapping and stream-drop abort verification.

mod support;

use philo_agent_runtime::{
    ModelEvent, ModelMessage, ModelPort, ModelToolCall, ModelToolResultOutcome, ToolCallId,
    UserPart,
};
use philo_model::{ModelProtocol, PhiloModelAdapter};
use support::{
    BodyDropFlag, StubResponse, StubTransport, adapter_over, collect_ok, read_tool_definition,
    snapshot, text_sse,
};

const CANCELLED_TEXT: &str =
    "cancelled: the tool call did not execute because the turn was cancelled";

fn user(content: &str) -> ModelMessage {
    ModelMessage::User {
        parts: vec![UserPart::Text(content.to_owned())],
    }
}

/// History of a cancelled turn: call-1 executed for real, call-2 never ran.
fn cancelled_turn_history() -> Vec<ModelMessage> {
    vec![
        user("read two files"),
        ModelMessage::AssistantToolCalls {
            calls: vec![
                ModelToolCall {
                    tool_call_id: ToolCallId::new("call-1"),
                    name: "read".to_owned(),
                    arguments: r#"{"path":"a.txt"}"#.to_owned(),
                },
                ModelToolCall {
                    tool_call_id: ToolCallId::new("call-2"),
                    name: "read".to_owned(),
                    arguments: r#"{"path":"b.txt"}"#.to_owned(),
                },
            ],
        },
        ModelMessage::ToolResult {
            tool_call_id: ToolCallId::new("call-1"),
            outcome: ModelToolResultOutcome::Success {
                content: "alpha".to_owned(),
            },
        },
        ModelMessage::ToolResult {
            tool_call_id: ToolCallId::new("call-2"),
            outcome: ModelToolResultOutcome::Cancelled,
        },
        user("continue"),
    ]
}

// --- Cancelled replay mapping -------------------------------------------------

#[tokio::test]
async fn cancelled_history_replays_canonical_text_without_native_error_status() {
    let transport =
        StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub-gpt", &["ok"]))]);
    let adapter = adapter_over(transport.clone());
    let stream = adapter
        .start(snapshot(
            cancelled_turn_history(),
            vec![read_tool_definition()],
        ))
        .await
        .expect("replay with a cancelled mark passes validation");
    let events = collect_ok(stream).await;
    assert!(events.contains(&ModelEvent::Completed));

    let body = &transport.request_bodies()[0];
    let real = &body["messages"][2];
    assert_eq!(real["role"], "tool");
    assert_eq!(real["tool_call_id"], "call-1");
    assert_eq!(real["content"][0]["text"], "alpha");

    // OpenAI Chat has no native error status: the canonical cancellation text
    // travels as plain result content (M4 adaptation precedent).
    let cancelled = &body["messages"][3];
    assert_eq!(cancelled["role"], "tool");
    assert_eq!(cancelled["tool_call_id"], "call-2");
    assert_eq!(cancelled["content"][0]["text"], CANCELLED_TEXT);
}

#[tokio::test]
async fn anthropic_renders_cancelled_with_the_native_error_status() {
    let body = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"id":"msg-1","model":"stub-claude","usage":{"input_tokens":1}}}"#,
        "\n\n",
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
        "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
        "\n\n",
        "event: content_block_stop\n",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "\n\n",
        "event: message_delta\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
        "\n\n",
        "event: message_stop\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );
    let transport = StubTransport::new([StubResponse::Sse(body.as_bytes().to_vec())]);
    let adapter = PhiloModelAdapter::builder(
        "stub-provider",
        ModelProtocol::AnthropicMessages,
        "stub-model",
        "https://stub.invalid/v1/messages",
    )
    .build_with_transport(transport.clone())
    .expect("anthropic adapter assembly");
    let stream = adapter
        .start(snapshot(
            cancelled_turn_history(),
            vec![read_tool_definition()],
        ))
        .await
        .expect("replay with a cancelled mark passes validation");
    let events = collect_ok(stream).await;
    assert!(events.contains(&ModelEvent::Completed));

    let request = &transport.request_bodies()[0];
    let results = &request["messages"][2];
    assert_eq!(results["role"], "user");
    let real = &results["content"][0];
    assert_eq!(real["type"], "tool_result");
    assert_eq!(real["tool_use_id"], "call-1");
    assert_eq!(real["content"][0]["text"], "alpha");
    let cancelled = &results["content"][1];
    assert_eq!(cancelled["type"], "tool_result");
    assert_eq!(cancelled["tool_use_id"], "call-2");
    assert_eq!(cancelled["is_error"], true, "native error status is used");
    assert_eq!(cancelled["content"][0]["text"], CANCELLED_TEXT);
}

// --- Stream drop aborts the underlying call -------------------------------------

#[tokio::test]
async fn dropping_the_stream_mid_flight_aborts_the_underlying_call() {
    let flag = BodyDropFlag::new();
    // The body carries the response head and one delta, then stays open
    // forever: the stream is genuinely mid-flight when dropped.
    let head = text_sse("resp-1", "stub-gpt", &["partial"]);
    let head = {
        // Keep only the first two SSE records (role chunk + one delta):
        // the terminator never arrives.
        let text = String::from_utf8(head).expect("sse is utf-8");
        let mut records = text.split_inclusive("\n\n");
        let mut kept = String::new();
        kept.push_str(records.next().expect("role record"));
        kept.push_str(records.next().expect("delta record"));
        kept.into_bytes()
    };
    let transport = StubTransport::new([StubResponse::SseSuspended {
        head,
        flag: flag.clone(),
    }]);
    let adapter = adapter_over(transport.clone());
    let mut stream = adapter
        .start(snapshot(vec![user("hi")], Vec::new()))
        .await
        .expect("call starts");

    // Consume the mid-flight events that already arrived.
    let first = stream.next().await.expect("stream is live").expect("ok");
    assert!(matches!(first, ModelEvent::ResponseStarted { .. }));
    let second = stream.next().await.expect("stream is live").expect("ok");
    assert!(matches!(second, ModelEvent::TextDelta(delta) if delta == "partial"));
    assert!(
        !flag.is_dropped(),
        "the connection is alive while the stream is held"
    );

    // Dropping the event stream is the cancellation signal: the transport
    // body must be released with it, aborting the underlying call.
    drop(stream);
    assert!(
        flag.is_dropped(),
        "dropping the stream must abort the transport body"
    );
    assert_eq!(
        transport.requests().len(),
        1,
        "no follow-up requests after the drop"
    );
}
