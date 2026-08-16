//! MODEL-006: `Interrupted` outcome rendering with status adaptation.
//! One rule covers durable seal results and synthesized placeholders alike;
//! histories without `Interrupted` keep the 0.9 request shape byte for byte.

mod support;

use philo_agent_runtime::{
    ModelEvent, ModelMessage, ModelPort, ModelToolCall, ModelToolResultOutcome, ToolCallId,
    UserPart,
};
use support::{
    StubResponse, StubTransport, adapter_over, collect_ok, read_tool_definition, snapshot, text_sse,
};

const INTERRUPTED_TEXT: &str = "interrupted: the process was interrupted while this call was \
     outstanding; whether it executed is unknown, so verify the actual state before assuming";

fn user(content: &str) -> ModelMessage {
    ModelMessage::User {
        parts: vec![UserPart::Text(content.to_owned())],
    }
}

/// History of a sealed crash remnant: both calls carry Interrupted marks.
fn sealed_turn_history() -> Vec<ModelMessage> {
    vec![
        user("edit two files"),
        ModelMessage::AssistantToolCalls {
            calls: vec![
                ModelToolCall {
                    tool_call_id: ToolCallId::new("call-1"),
                    name: "write".to_owned(),
                    arguments: r#"{"path":"a.txt"}"#.to_owned(),
                },
                ModelToolCall {
                    tool_call_id: ToolCallId::new("call-2"),
                    name: "shell".to_owned(),
                    arguments: r#"{"command":"ls"}"#.to_owned(),
                },
            ],
        },
        ModelMessage::ToolResult {
            tool_call_id: ToolCallId::new("call-1"),
            outcome: ModelToolResultOutcome::Interrupted,
        },
        ModelMessage::ToolResult {
            tool_call_id: ToolCallId::new("call-2"),
            outcome: ModelToolResultOutcome::Interrupted,
        },
        user("continue"),
    ]
}

#[tokio::test]
async fn interrupted_history_replays_canonical_text_without_native_error_status() {
    let transport =
        StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub-gpt", &["ok"]))]);
    let adapter = adapter_over(transport.clone());
    let stream = adapter
        .start(snapshot(
            sealed_turn_history(),
            vec![read_tool_definition()],
        ))
        .await
        .expect("replay with interrupted marks passes validation");
    let events = collect_ok(stream).await;
    assert!(events.contains(&ModelEvent::Completed));

    // OpenAI Chat has no native error status: the canonical interrupted
    // text travels as plain result content (M4 adaptation precedent).
    let body = &transport.request_bodies()[0];
    for (index, call_id) in [(2, "call-1"), (3, "call-2")] {
        let message = &body["messages"][index];
        assert_eq!(message["role"], "tool");
        assert_eq!(message["tool_call_id"], call_id);
        assert_eq!(message["content"][0]["text"], INTERRUPTED_TEXT);
    }
}

#[tokio::test]
async fn histories_without_interrupted_keep_the_previous_request_shape() {
    // A mixed real/cancelled history (the 0.9 shape) must serialize exactly
    // as before: the new variant only adds a branch.
    let transport =
        StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub-gpt", &["ok"]))]);
    let adapter = adapter_over(transport.clone());
    let history = vec![
        user("read a file"),
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
        user("continue"),
    ];
    let stream = adapter
        .start(snapshot(history, vec![read_tool_definition()]))
        .await
        .expect("call starts");
    collect_ok(stream).await;

    let body = &transport.request_bodies()[0];
    let result = &body["messages"][2];
    assert_eq!(result["role"], "tool");
    assert_eq!(result["content"][0]["text"], "alpha");
    let serialized = body.to_string();
    assert!(
        !serialized.contains("interrupted"),
        "no interrupted text leaks into unrelated requests"
    );
}
