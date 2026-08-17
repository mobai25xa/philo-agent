//! MODEL-004: multi-part user message mapping onto SDK `UserContent`,
//! image validation failure normalization, and multipart history replay.

mod support;

use philo_agent_runtime::{
    ModelMessage, ModelPort, ModelToolCall, ModelToolResultOutcome, ToolCallId, UserPart,
};
use support::{
    StubResponse, StubTransport, adapter_over, assistant_tool_calls, collect_ok, snapshot, text_sse,
};

/// Image bytes `abc`; base64 `YWJj`.
const IMAGE_BYTES: &[u8] = b"abc";
const IMAGE_DATA_URL: &str = "data:image/png;base64,YWJj";

fn image_part(media_type: &str) -> UserPart {
    UserPart::Image {
        media_type: media_type.to_owned(),
        bytes: IMAGE_BYTES.to_vec(),
    }
}

fn mixed_user() -> ModelMessage {
    ModelMessage::User {
        parts: vec![
            UserPart::Text("what is in this picture?".to_owned()),
            image_part("image/png"),
        ],
    }
}

// --- parts -> UserContent mapping (M8-006) --------------------------------------

#[tokio::test]
async fn image_parts_map_to_inline_data_urls() {
    let transport = StubTransport::new([StubResponse::Sse(text_sse(
        "resp-1",
        "stub-gpt",
        &["a cat"],
    ))]);
    let adapter = adapter_over(transport.clone());
    let stream = adapter
        .start(snapshot(vec![mixed_user()], Vec::new()))
        .await
        .expect("call starts");
    collect_ok(stream).await;

    let body = &transport.request_bodies()[0];
    let content = &body["messages"][0]["content"];
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "what is in this picture?");
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(
        content[1]["image_url"]["url"], IMAGE_DATA_URL,
        "bytes are forwarded verbatim as a base64 data URL"
    );
}

#[tokio::test]
async fn image_only_user_message_maps_without_text_parts() {
    let transport = StubTransport::new([StubResponse::Sse(text_sse(
        "resp-1",
        "stub-gpt",
        &["a dog"],
    ))]);
    let adapter = adapter_over(transport.clone());
    let stream = adapter
        .start(snapshot(
            vec![ModelMessage::User {
                parts: vec![image_part("image/jpeg")],
            }],
            Vec::new(),
        ))
        .await
        .expect("call starts");
    collect_ok(stream).await;

    let content = &transport.request_bodies()[0]["messages"][0]["content"];
    assert_eq!(content.as_array().expect("parts array").len(), 1);
    assert_eq!(content[0]["type"], "image_url");
    assert_eq!(
        content[0]["image_url"]["url"],
        "data:image/jpeg;base64,YWJj"
    );
}

// --- Multipart history replays across rounds (M8-006) ---------------------------

#[tokio::test]
async fn multipart_history_replays_in_later_round_requests() {
    let transport =
        StubTransport::new([StubResponse::Sse(text_sse("resp-2", "stub-gpt", &["done"]))]);
    let adapter = adapter_over(transport.clone());
    // A second-round snapshot: the multipart user message followed by the
    // first round's tool exchange.
    let stream = adapter
        .start(snapshot(
            vec![
                mixed_user(),
                assistant_tool_calls([ModelToolCall {
                    tool_call_id: ToolCallId::new("call-1"),
                    name: "read".to_owned(),
                    arguments: r#"{"path":"a.txt"}"#.to_owned(),
                }]),
                ModelMessage::ToolResult {
                    tool_call_id: ToolCallId::new("call-1"),
                    outcome: ModelToolResultOutcome::Success {
                        content: "alpha".to_owned(),
                    },
                },
            ],
            Vec::new(),
        ))
        .await
        .expect("call starts");
    collect_ok(stream).await;

    let body = &transport.request_bodies()[0];
    // The replayed multipart message keeps its image data URL ahead of the
    // tool exchange.
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(
        body["messages"][0]["content"][1]["image_url"]["url"],
        IMAGE_DATA_URL
    );
    assert_eq!(body["messages"][1]["role"], "assistant");
    assert_eq!(body["messages"][2]["role"], "tool");
}

// --- Validation failures normalize before any request (M8-006) ------------------

#[tokio::test]
async fn illegal_media_type_is_a_configuration_error_before_send() {
    let transport = StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub-gpt", &["x"]))]);
    let adapter = adapter_over(transport.clone());
    let Err(error) = adapter
        .start(snapshot(
            vec![ModelMessage::User {
                parts: vec![image_part("text/plain")],
            }],
            Vec::new(),
        ))
        .await
    else {
        panic!("non-image media type must be rejected");
    };

    assert!(
        error.message().contains("user image rejected"),
        "configuration-class diagnostic: {}",
        error.message()
    );
    assert!(
        transport.requests().is_empty(),
        "mapping failures never produce a partial request"
    );
}

#[tokio::test]
async fn empty_image_bytes_are_rejected_before_send() {
    let transport = StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub-gpt", &["x"]))]);
    let adapter = adapter_over(transport.clone());
    let Err(error) = adapter
        .start(snapshot(
            vec![ModelMessage::User {
                parts: vec![UserPart::Image {
                    media_type: "image/png".to_owned(),
                    bytes: Vec::new(),
                }],
            }],
            Vec::new(),
        ))
        .await
    else {
        panic!("empty image bytes must be rejected");
    };

    assert!(error.message().contains("user image rejected"));
    assert!(transport.requests().is_empty());
}
