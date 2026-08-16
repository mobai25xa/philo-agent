//! Phase 3 acceptance: explicit server continuation is a target-bound fast
//! path over the durable local replay source of truth.

mod support;

use std::sync::Arc;

use http::StatusCode;
use philo_agent_runtime::{ModelEvent, ModelMessage, ModelPort};
use philo_model::{
    MemoryModelReplayStore, ModelCompat, ModelContinuationPolicy, ModelProtocol, ModelReplayStore,
    PhiloModelAdapter,
};
use serde_json::Value;
use support::{StubResponse, StubTransport, collect, collect_ok, reasoning_snapshot};

const STUB_RESPONSES_ENDPOINT: &str = "https://stub.invalid/v1/responses";
const OFFICIAL_RESPONSES_ENDPOINT: &str = "https://api.openai.com/v1/responses";
const MINIMAL_RESPONSE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../philo/crates/philo/tests/fixtures/openai_responses/stream/minimal.sse"
));
const COMPAT_MINIMAL_RESPONSE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../philo/crates/philo/tests/fixtures/openai_responses/stream/compat-minimal.sse"
));

fn user(text: &str) -> ModelMessage {
    ModelMessage::User {
        parts: vec![philo_agent_runtime::UserPart::Text(text.to_owned())],
    }
}

fn snapshot(turn: &str, messages: Vec<ModelMessage>) -> philo_agent_runtime::ModelCallSnapshot {
    reasoning_snapshot(turn, 1, None, messages, Vec::new())
}

fn official_adapter(
    transport: StubTransport,
    store: Arc<dyn ModelReplayStore>,
    model: &str,
    policy: ModelContinuationPolicy,
) -> PhiloModelAdapter {
    let endpoint = if policy == ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback {
        OFFICIAL_RESPONSES_ENDPOINT
    } else {
        STUB_RESPONSES_ENDPOINT
    };
    PhiloModelAdapter::builder(
        "stub-provider",
        ModelProtocol::OpenAiResponses,
        model,
        endpoint,
    )
    .compat(ModelCompat::Official)
    .replay_store(store)
    .continuation_policy(policy)
    .build_with_transport(transport)
    .expect("official Responses adapter assembly")
}

fn compatible_adapter(
    transport: StubTransport,
    store: Arc<dyn ModelReplayStore>,
    model: &str,
    policy: ModelContinuationPolicy,
) -> PhiloModelAdapter {
    PhiloModelAdapter::builder(
        "stub-provider",
        ModelProtocol::OpenAiResponses,
        model,
        STUB_RESPONSES_ENDPOINT,
    )
    .compat(ModelCompat::Compatible)
    .replay_store(store)
    .continuation_policy(policy)
    .build_with_transport(transport)
    .expect("compatible Responses adapter assembly")
}

fn unavailable_previous_response() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "error": {
            "message": "stored response is unavailable",
            "type": "invalid_request_error",
            "code": "previous_response_not_found"
        }
    }))
    .expect("error JSON")
}

#[tokio::test]
async fn default_policy_remains_stateless_and_sends_the_full_history() {
    let transport = StubTransport::new([StubResponse::Sse(MINIMAL_RESPONSE.to_vec())]);
    let model = official_adapter(
        transport.clone(),
        Arc::new(MemoryModelReplayStore::default()),
        "stub-model",
        ModelContinuationPolicy::StatelessLocalReplay,
    );
    collect_ok(
        model
            .start(snapshot("turn-1", vec![user("hello")]))
            .await
            .expect("call starts"),
    )
    .await;

    let body = &transport.request_bodies()[0];
    assert_eq!(body["store"], false);
    assert!(body.get("previous_response_id").is_none());
    assert_eq!(body["input"].as_array().expect("input").len(), 1);
}

#[tokio::test]
async fn completed_response_continues_across_adapter_instances_with_only_new_input() {
    let store: Arc<dyn ModelReplayStore> = Arc::new(MemoryModelReplayStore::default());
    let first_transport = StubTransport::new([StubResponse::Sse(MINIMAL_RESPONSE.to_vec())]);
    let first = official_adapter(
        first_transport.clone(),
        store.clone(),
        "stub-model",
        ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback,
    );
    collect_ok(
        first
            .start(snapshot("turn-1", vec![user("first")]))
            .await
            .expect("first call starts"),
    )
    .await;
    assert_eq!(first_transport.request_bodies()[0]["store"], true);

    let second_transport = StubTransport::new([StubResponse::Sse(MINIMAL_RESPONSE.to_vec())]);
    let second = official_adapter(
        second_transport.clone(),
        store,
        "stub-model",
        ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback,
    );
    collect_ok(
        second
            .start(snapshot(
                "turn-2",
                vec![
                    user("first"),
                    ModelMessage::Assistant {
                        content: "hello".to_owned(),
                    },
                    user("second"),
                ],
            ))
            .await
            .expect("continued call starts"),
    )
    .await;

    let body = &second_transport.request_bodies()[0];
    assert_eq!(body["store"], true);
    assert_eq!(body["previous_response_id"], "resp-fixture");
    let input = body["input"].as_array().expect("input");
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["content"][0]["text"], "second");
}

#[tokio::test]
async fn target_switch_starts_a_new_full_chain_without_reusing_the_old_id() {
    let store: Arc<dyn ModelReplayStore> = Arc::new(MemoryModelReplayStore::default());
    let first = official_adapter(
        StubTransport::new([StubResponse::Sse(MINIMAL_RESPONSE.to_vec())]),
        store.clone(),
        "model-a",
        ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback,
    );
    collect_ok(
        first
            .start(snapshot("turn-1", vec![user("first")]))
            .await
            .expect("first call starts"),
    )
    .await;

    let transport = StubTransport::new([StubResponse::Sse(MINIMAL_RESPONSE.to_vec())]);
    let switched = official_adapter(
        transport.clone(),
        store,
        "model-b",
        ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback,
    );
    collect_ok(
        switched
            .start(snapshot(
                "turn-2",
                vec![
                    user("first"),
                    ModelMessage::Assistant {
                        content: "hello".to_owned(),
                    },
                    user("second"),
                ],
            ))
            .await
            .expect("switched target starts"),
    )
    .await;

    let body = &transport.request_bodies()[0];
    assert!(body.get("previous_response_id").is_none());
    assert_eq!(body["store"], true);
    assert_eq!(body["input"].as_array().expect("input").len(), 3);
}

#[tokio::test]
async fn unavailable_chain_falls_back_once_and_does_not_reactivate_the_old_id() {
    let store: Arc<dyn ModelReplayStore> = Arc::new(MemoryModelReplayStore::default());
    let transport = StubTransport::new([
        StubResponse::Sse(MINIMAL_RESPONSE.to_vec()),
        StubResponse::Status(StatusCode::BAD_REQUEST, unavailable_previous_response()),
        StubResponse::Sse(MINIMAL_RESPONSE.to_vec()),
        StubResponse::Sse(MINIMAL_RESPONSE.to_vec()),
    ]);
    let model = official_adapter(
        transport.clone(),
        store.clone(),
        "stub-model",
        ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback,
    );
    collect_ok(
        model
            .start(snapshot("turn-1", vec![user("first")]))
            .await
            .expect("first call starts"),
    )
    .await;

    let second_history = vec![
        user("first"),
        ModelMessage::Assistant {
            content: "hello".to_owned(),
        },
        user("second"),
    ];
    collect_ok(
        model
            .start(snapshot("turn-2", second_history.clone()))
            .await
            .expect("fallback call starts"),
    )
    .await;

    let mut third_history = second_history;
    third_history.push(ModelMessage::Assistant {
        content: "hello".to_owned(),
    });
    third_history.push(user("third"));
    let restarted = official_adapter(
        transport.clone(),
        store.clone(),
        "stub-model",
        ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback,
    );
    collect_ok(
        restarted
            .start(snapshot("turn-3", third_history))
            .await
            .expect("new chain starts"),
    )
    .await;

    let bodies = transport.request_bodies();
    assert_eq!(bodies.len(), 4, "one failed continuation plus one fallback");
    assert_eq!(bodies[1]["previous_response_id"], "resp-fixture");
    assert_eq!(bodies[1]["store"], true);
    assert_eq!(bodies[1]["input"].as_array().expect("input").len(), 1);
    assert!(bodies[2].get("previous_response_id").is_none());
    assert_eq!(bodies[2]["store"], false);
    assert_eq!(bodies[2]["input"].as_array().expect("input").len(), 3);
    assert!(bodies[3].get("previous_response_id").is_none());
    assert_eq!(bodies[3]["store"], true);
    assert_eq!(bodies[3]["input"].as_array().expect("input").len(), 5);

    let tombstone = store
        .load("session-1")
        .expect("load replay store")
        .into_iter()
        .map(|blob| serde_json::from_slice::<Value>(blob.payload()).expect("stored JSON"))
        .find(|value| !value["invalidates_generation"].is_null())
        .expect("continuation tombstone");
    assert!(tombstone["response_id"].is_null());
    assert_eq!(tombstone["items"], serde_json::json!([]));
    assert!(!tombstone.to_string().contains("resp-fixture"));
}

#[tokio::test]
async fn changed_prefix_and_incomplete_response_never_activate_a_stale_id() {
    let store: Arc<dyn ModelReplayStore> = Arc::new(MemoryModelReplayStore::default());
    let first = official_adapter(
        StubTransport::new([StubResponse::Sse(MINIMAL_RESPONSE.to_vec())]),
        store.clone(),
        "stub-model",
        ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback,
    );
    collect_ok(
        first
            .start(snapshot("turn-1", vec![user("original")]))
            .await
            .expect("first call starts"),
    )
    .await;

    let changed_transport = StubTransport::new([StubResponse::Sse(MINIMAL_RESPONSE.to_vec())]);
    let changed = official_adapter(
        changed_transport.clone(),
        store.clone(),
        "stub-model",
        ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback,
    );
    collect_ok(
        changed
            .start(snapshot(
                "turn-2",
                vec![
                    user("different branch"),
                    ModelMessage::Assistant {
                        content: "hello".to_owned(),
                    },
                    user("next"),
                ],
            ))
            .await
            .expect("changed branch starts"),
    )
    .await;
    let changed_body = &changed_transport.request_bodies()[0];
    assert!(changed_body.get("previous_response_id").is_none());
    assert_eq!(changed_body["store"], true);
    assert_eq!(changed_body["input"].as_array().expect("input").len(), 3);

    let terminal = MINIMAL_RESPONSE
        .windows(b"data: ".len())
        .rposition(|window| window == b"data: ")
        .expect("terminal response event");
    let incomplete_transport = StubTransport::new([
        StubResponse::Sse(MINIMAL_RESPONSE[..terminal].to_vec()),
        StubResponse::Sse(MINIMAL_RESPONSE.to_vec()),
    ]);
    let incomplete = official_adapter(
        incomplete_transport.clone(),
        Arc::new(MemoryModelReplayStore::default()),
        "stub-model",
        ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback,
    );
    let events = collect(
        incomplete
            .start(snapshot("incomplete", vec![user("partial")]))
            .await
            .expect("incomplete stream starts"),
    )
    .await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ModelEvent::Completed)))
    );
    collect_ok(
        incomplete
            .start(snapshot(
                "after-incomplete",
                vec![
                    user("partial"),
                    ModelMessage::Assistant {
                        content: "hello".to_owned(),
                    },
                    user("retry"),
                ],
            ))
            .await
            .expect("next call starts"),
    )
    .await;
    let after = &incomplete_transport.request_bodies()[1];
    assert!(after.get("previous_response_id").is_none());
    assert_eq!(after["store"], true);
    assert_eq!(after["input"].as_array().expect("input").len(), 3);
}

#[tokio::test]
async fn compatible_continuation_uses_reconstruct_fixture_not_official_lifecycle() {
    let store: Arc<dyn ModelReplayStore> = Arc::new(MemoryModelReplayStore::default());
    let first_transport = StubTransport::new([StubResponse::Sse(COMPAT_MINIMAL_RESPONSE.to_vec())]);
    let first = compatible_adapter(
        first_transport.clone(),
        store.clone(),
        "stub-model",
        ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback,
    );
    collect_ok(
        first
            .start(snapshot("turn-1", vec![user("first")]))
            .await
            .expect("first call starts"),
    )
    .await;
    let first_body = &first_transport.request_bodies()[0];
    assert_eq!(first_body["store"], true);
    assert!(first_body.get("truncation").is_none());
    assert!(first_body.get("previous_response_id").is_none());

    let second_transport = StubTransport::new([StubResponse::Sse(COMPAT_MINIMAL_RESPONSE.to_vec())]);
    let second = compatible_adapter(
        second_transport.clone(),
        store,
        "stub-model",
        ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback,
    );
    collect_ok(
        second
            .start(snapshot(
                "turn-2",
                vec![
                    user("first"),
                    ModelMessage::Assistant {
                        content: "hello".to_owned(),
                    },
                    user("second"),
                ],
            ))
            .await
            .expect("continued call starts"),
    )
    .await;

    let body = &second_transport.request_bodies()[0];
    assert_eq!(body["store"], true);
    assert_eq!(body["previous_response_id"], "resp-fixture");
    let input = body["input"].as_array().expect("input");
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["content"][0]["text"], "second");
    assert!(input.iter().all(|item| item["type"] != "reasoning"));
}

#[test]
fn continuation_assembly_is_validated() {
    let official_on_compatible_host = PhiloModelAdapter::builder(
        "provider",
        ModelProtocol::OpenAiResponses,
        "model",
        STUB_RESPONSES_ENDPOINT,
    )
    .compat(ModelCompat::Official)
    .continuation_policy(ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback)
    .build_with_transport(StubTransport::new([]));
    assert!(
        official_on_compatible_host
            .err()
            .expect("official prefer must verify its endpoint host")
            .message()
            .contains("api.openai.com")
    );

    let chat_with_prefer = PhiloModelAdapter::builder(
        "provider",
        ModelProtocol::OpenAiChat,
        "model",
        "https://stub.invalid/v1/chat/completions",
    )
    .continuation_policy(ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback)
    .build_with_transport(StubTransport::new([]));
    assert!(
        chat_with_prefer
            .err()
            .expect("prefer continuation is Responses-only")
            .message()
            .contains("OpenAI Responses protocol")
    );
}
