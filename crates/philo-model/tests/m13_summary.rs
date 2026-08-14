//! MODEL-007: durable summaries use the SDK instructions channel and do not
//! change the request shape of histories without a summary.

mod support;

use philo_agent_runtime::{ModelMessage, ModelPort, UserPart};
use support::{StubResponse, StubTransport, adapter_over, collect_ok, snapshot, text_sse};

fn system(content: &str) -> ModelMessage {
    ModelMessage::System {
        content: content.to_owned(),
    }
}

fn summary(text: &str) -> ModelMessage {
    ModelMessage::Summary {
        text: text.to_owned(),
    }
}

fn user(content: &str) -> ModelMessage {
    ModelMessage::User {
        parts: vec![UserPart::Text(content.to_owned())],
    }
}

#[tokio::test]
async fn compacted_history_places_summary_after_system_and_preserves_tail_order() {
    let transport =
        StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub-gpt", &["ok"]))]);
    let adapter = adapter_over(transport.clone());
    let stream = adapter
        .start(snapshot(
            vec![
                system("main system prompt"),
                summary("The user chose the compact layout."),
                user("What did I choose?"),
                ModelMessage::Assistant {
                    content: "The compact layout.".to_owned(),
                },
                user("Continue."),
            ],
            Vec::new(),
        ))
        .await
        .expect("call starts");
    collect_ok(stream).await;

    let body = &transport.request_bodies()[0];
    assert_eq!(body["messages"].as_array().expect("messages").len(), 4);
    assert_eq!(body["messages"][0]["role"], "developer");
    assert_eq!(
        body["messages"][0]["content"],
        "main system prompt\n\nSummary of earlier conversation:\n\
         The user chose the compact layout."
    );
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(
        body["messages"][1]["content"][0]["text"],
        "What did I choose?"
    );
    assert_eq!(body["messages"][2]["role"], "assistant");
    assert_eq!(
        body["messages"][2]["content"][0]["text"],
        "The compact layout."
    );
    assert_eq!(body["messages"][3]["role"], "user");
    assert_eq!(body["messages"][3]["content"][0]["text"], "Continue.");
}

#[tokio::test]
async fn request_without_summary_matches_the_pre_m13_wire_golden_byte_for_byte() {
    let transport =
        StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub-gpt", &["ok"]))]);
    let adapter = adapter_over(transport.clone());
    let stream = adapter
        .start(snapshot(
            vec![system("be helpful"), user("hello")],
            Vec::new(),
        ))
        .await
        .expect("call starts");
    collect_ok(stream).await;

    let request = &transport.requests()[0];
    const PRE_M13_GOLDEN: &[u8] = br#"{"model":"stub-model","messages":[{"role":"developer","content":"be helpful"},{"role":"user","content":[{"type":"text","text":"hello"}]}],"max_completion_tokens":256,"stream":true,"n":1,"stream_options":{"include_usage":true},"temperature":0.25}"#;
    let body: &[u8] = request.body.as_ref();
    assert_eq!(body, PRE_M13_GOLDEN);
}
