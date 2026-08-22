//! MODEL-008: sidecar persistent replay lives in a private sidecar, survives
//! fresh SDK/adapter instances, and is committed before runtime completion.
//!
//! The JSONL restart test drives Runtime through `RuntimeHandle::submit` and
//! subscription drain.

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use philo_agent_runtime::{
    GenerationConfig, IdSource, ModelEvent, ModelMessage, ModelPort, ModelToolCall,
    ModelToolResultOutcome, OperationOutcome, RuntimeConfig, SequentialIdSource, SessionId,
    ShutdownMode, ToolCallId, ToolRegistry, UserMessage, UserPart,
};
use philo_model::{
    FileModelReplayStore, MemoryModelReplayStore, ModelCompat, ModelProtocol, ModelReplayStore,
    PhiloModelAdapter, ReplayStoreBlob, ReplayStoreError, ReplayStoreErrorCode, ReplayStorePolicy,
};
use philo_session_jsonl::JsonlSessionStore;
use philo_tools_std::ReadTool;
use serde_json::{Value, json};
use support::{
    StubResponse, StubTransport, assistant_tool_calls, collect, collect_ok, drain_until_settled,
    reasoning_snapshot, sse, start_runtime, submit_prompt, test_generation,
};

const RESPONSES_ENDPOINT: &str = "https://stub.invalid/v1/responses";
const MINIMAL_RESPONSE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../philo/crates/philo/tests/fixtures/openai_responses/stream/minimal.sse"
));

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn user(text: &str) -> ModelMessage {
    ModelMessage::User {
        parts: vec![UserPart::Text(text.to_owned())],
    }
}

fn tool_history(calls: &[(&str, &str, &str)]) -> Vec<ModelMessage> {
    let model_calls = calls
        .iter()
        .map(|(call_id, name, arguments)| ModelToolCall {
            tool_call_id: ToolCallId::new(*call_id),
            name: (*name).to_owned(),
            arguments: (*arguments).to_owned(),
        })
        .collect::<Vec<_>>();
    let mut messages = vec![user("use the tools"), assistant_tool_calls(model_calls)];
    messages.extend(
        calls
            .iter()
            .map(|(call_id, _, _)| ModelMessage::ToolResult {
                tool_call_id: ToolCallId::new(*call_id),
                outcome: ModelToolResultOutcome::Success {
                    content: format!("result for {call_id}"),
                },
            }),
    );
    messages
}

fn responses_tool_body(response_id: &str, opaque: &str, calls: &[(&str, &str, &str)]) -> Vec<u8> {
    let mut sequence = 0_u64;
    let mut records = Vec::new();
    records.push(
        json!({
            "type": "response.created",
            "sequence_number": sequence,
            "response": {"id": response_id, "model": "fixture-model"}
        })
        .to_string(),
    );
    sequence += 1;
    records.push(
        json!({
            "type": "response.output_item.added",
            "sequence_number": sequence,
            "output_index": 0,
            "item": {
                "id": "reasoning-item",
                "type": "reasoning",
                "summary": [],
                "encrypted_content": opaque
            }
        })
        .to_string(),
    );
    sequence += 1;
    records.push(
        json!({
            "type": "response.output_item.done",
            "sequence_number": sequence,
            "output_index": 0,
            "item": {
                "id": "reasoning-item",
                "type": "reasoning",
                "summary": [],
                "encrypted_content": opaque
            }
        })
        .to_string(),
    );
    sequence += 1;

    for (offset, (call_id, name, arguments)) in calls.iter().enumerate() {
        let output_index = u32::try_from(offset + 1).expect("small fixture");
        let item_id = format!("function-item-{offset}");
        records.push(
            json!({
                "type": "response.output_item.added",
                "sequence_number": sequence,
                "output_index": output_index,
                "item": {
                    "id": item_id,
                    "type": "function_call",
                    "status": "in_progress",
                    "arguments": "",
                    "call_id": call_id,
                    "name": name
                }
            })
            .to_string(),
        );
        sequence += 1;
        records.push(
            json!({
                "type": "response.function_call_arguments.delta",
                "sequence_number": sequence,
                "item_id": item_id,
                "output_index": output_index,
                "delta": arguments
            })
            .to_string(),
        );
        sequence += 1;
        records.push(
            json!({
                "type": "response.function_call_arguments.done",
                "sequence_number": sequence,
                "item_id": item_id,
                "output_index": output_index,
                "arguments": arguments
            })
            .to_string(),
        );
        sequence += 1;
        records.push(
            json!({
                "type": "response.output_item.done",
                "sequence_number": sequence,
                "output_index": output_index,
                "item": {
                    "id": item_id,
                    "type": "function_call",
                    "status": "completed",
                    "arguments": arguments,
                    "call_id": call_id,
                    "name": name
                }
            })
            .to_string(),
        );
        sequence += 1;
    }
    records.push(
        json!({
            "type": "response.completed",
            "sequence_number": sequence,
            "response": {
                "id": response_id,
                "model": "fixture-model",
                "status": "completed"
            }
        })
        .to_string(),
    );
    let records = records.iter().map(String::as_str).collect::<Vec<_>>();
    sse(&records)
}

fn response_adapter(
    transport: StubTransport,
    store: Arc<dyn ModelReplayStore>,
    model: &str,
) -> PhiloModelAdapter {
    PhiloModelAdapter::builder(
        "stub-provider",
        ModelProtocol::OpenAiResponses,
        model,
        RESPONSES_ENDPOINT,
    )
    .compat(ModelCompat::Official)
    .replay_store(store)
    .build_with_transport(transport)
    .expect("Responses adapter assembly")
}

#[tokio::test]
async fn response_items_restore_in_order_through_a_fresh_adapter() {
    let root = TempRoot::new("phase2-restart");
    let calls = [
        ("call-1", "read", r#"{"path":"a.txt"}"#),
        ("call-2", "read", r#"{"path":"b.txt"}"#),
    ];
    let first_store = Arc::new(FileModelReplayStore::open(&root.path).expect("open replay store"));
    let first_transport = StubTransport::new([StubResponse::Sse(responses_tool_body(
        "response-1",
        "opaque-phase2-state",
        &calls,
    ))]);
    let first = response_adapter(first_transport, first_store, "stub-model");
    let events = collect_ok(
        first
            .start(reasoning_snapshot(
                "turn-1",
                1,
                None,
                vec![user("use the tools")],
                vec![support::read_tool_definition()],
            ))
            .await
            .expect("first call starts"),
    )
    .await;
    assert!(matches!(events.last(), Some(ModelEvent::Completed { .. })));
    drop(first);

    let second_store =
        Arc::new(FileModelReplayStore::open(&root.path).expect("reopen replay store"));
    let second_transport = StubTransport::new([StubResponse::Sse(MINIMAL_RESPONSE.to_vec())]);
    let second = response_adapter(second_transport.clone(), second_store, "stub-model");
    collect_ok(
        second
            .start(reasoning_snapshot(
                "turn-2",
                1,
                None,
                tool_history(&calls),
                vec![support::read_tool_definition()],
            ))
            .await
            .expect("restored call starts"),
    )
    .await;

    let body = &second_transport.request_bodies()[0];
    assert_eq!(body["input"][1]["type"], "reasoning");
    assert_eq!(body["input"][1]["encrypted_content"], "opaque-phase2-state");
    assert_eq!(body["input"][2]["call_id"], "call-1");
    assert_eq!(body["input"][3]["call_id"], "call-2");
    assert_eq!(body["input"][4]["call_id"], "call-1");
    assert_eq!(body["input"][5]["call_id"], "call-2");
}

#[tokio::test]
async fn target_switch_uses_portable_tool_fallback_without_raw_reasoning() {
    let root = TempRoot::new("phase2-target");
    let calls = [("call-1", "read", r#"{"path":"a.txt"}"#)];
    let store = Arc::new(FileModelReplayStore::open(&root.path).expect("open replay store"));
    let first = response_adapter(
        StubTransport::new([StubResponse::Sse(responses_tool_body(
            "response-1",
            "target-bound-secret",
            &calls,
        ))]),
        store,
        "stub-model",
    );
    collect_ok(
        first
            .start(reasoning_snapshot(
                "turn-1",
                1,
                None,
                vec![user("use the tool")],
                vec![support::read_tool_definition()],
            ))
            .await
            .expect("first call starts"),
    )
    .await;

    let reopened = Arc::new(FileModelReplayStore::open(&root.path).expect("reopen replay store"));
    let transport = StubTransport::new([StubResponse::Sse(MINIMAL_RESPONSE.to_vec())]);
    let switched = response_adapter(transport.clone(), reopened, "other-model");
    collect_ok(
        switched
            .start(reasoning_snapshot(
                "turn-2",
                1,
                None,
                tool_history(&calls),
                vec![support::read_tool_definition()],
            ))
            .await
            .expect("fallback call starts"),
    )
    .await;

    let bodies = transport.request_bodies();
    let input = bodies[0]["input"].as_array().expect("Responses input");
    assert!(input.iter().all(|item| item["type"] != "reasoning"));
    assert!(input.iter().all(|item| {
        item.get("encrypted_content") != Some(&Value::String("target-bound-secret".to_owned()))
    }));
    assert!(input.iter().any(|item| item["call_id"] == "call-1"));
}

#[tokio::test]
async fn corrupted_required_sidecar_fails_before_the_next_request() {
    let root = TempRoot::new("phase2-corrupt");
    let calls = [("call-1", "read", r#"{"path":"a.txt"}"#)];
    let store = Arc::new(FileModelReplayStore::open(&root.path).expect("open replay store"));
    let first = response_adapter(
        StubTransport::new([StubResponse::Sse(responses_tool_body(
            "response-1",
            "corruption-secret",
            &calls,
        ))]),
        store,
        "stub-model",
    );
    collect_ok(
        first
            .start(reasoning_snapshot(
                "turn-1",
                1,
                None,
                vec![user("use the tool")],
                vec![support::read_tool_definition()],
            ))
            .await
            .expect("first call starts"),
    )
    .await;

    let replay_dir = root.path.join("s-session-1").join("model-replay");
    let record = std::fs::read_dir(&replay_dir)
        .expect("read sidecar")
        .map(|entry| entry.expect("sidecar entry").path())
        .find(|path| path.extension().and_then(|value| value.to_str()) == Some("replay"))
        .expect("replay record");
    std::fs::write(record, b"{corrupted").expect("corrupt replay record");

    let transport = StubTransport::new([StubResponse::Sse(MINIMAL_RESPONSE.to_vec())]);
    let reopened = Arc::new(FileModelReplayStore::open(&root.path).expect("reopen replay store"));
    let adapter = response_adapter(transport.clone(), reopened, "stub-model");
    let error = adapter
        .start(reasoning_snapshot(
            "turn-2",
            1,
            None,
            tool_history(&calls),
            vec![support::read_tool_definition()],
        ))
        .await
        .err()
        .expect("corrupt required replay blocks the request");
    assert!(error.message().contains("sidecar is corrupted"));
    assert!(!error.message().contains("corruption-secret"));
    assert!(transport.requests().is_empty());
}

#[tokio::test]
async fn incomplete_response_and_disabled_maintenance_call_commit_nothing() {
    let root = TempRoot::new("phase2-incomplete");
    let store = Arc::new(FileModelReplayStore::open(&root.path).expect("open replay store"));
    let calls = [("call-1", "read", r#"{"path":"a.txt"}"#)];
    let complete = responses_tool_body("response-1", "never-committed", &calls);
    let terminal = complete
        .windows(b"data: ".len())
        .rposition(|window| window == b"data: ")
        .expect("terminal event marker");
    let transport = StubTransport::new([StubResponse::Sse(complete[..terminal].to_vec())]);
    let adapter = response_adapter(transport, store.clone(), "stub-model");
    let events = collect(
        adapter
            .start(reasoning_snapshot(
                "turn-1",
                1,
                None,
                vec![user("use the tool")],
                vec![support::read_tool_definition()],
            ))
            .await
            .expect("incomplete call starts"),
    )
    .await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ModelEvent::Completed { .. })))
    );
    assert!(
        store
            .load("session-1")
            .await
            .expect("load store")
            .is_empty()
    );

    let maintenance_transport = StubTransport::new([StubResponse::Sse(responses_tool_body(
        "response-2",
        "maintenance-only",
        &calls,
    ))]);
    let maintenance = response_adapter(maintenance_transport, store.clone(), "stub-model");
    let mut snapshot = reasoning_snapshot(
        "maintenance",
        1,
        None,
        vec![user("summarize")],
        vec![support::read_tool_definition()],
    );
    snapshot.persist_replay = false;
    collect_ok(
        maintenance
            .start(snapshot)
            .await
            .expect("maintenance call starts"),
    )
    .await;
    assert!(
        store
            .load("session-1")
            .await
            .expect("load store")
            .is_empty()
    );
}

#[derive(Debug)]
struct CommitFailureStore {
    inner: MemoryModelReplayStore,
}

impl CommitFailureStore {
    fn new() -> Self {
        Self {
            inner: MemoryModelReplayStore::default(),
        }
    }
}

impl ModelReplayStore for CommitFailureStore {
    fn policy(&self) -> ReplayStorePolicy {
        self.inner.policy()
    }

    fn load(
        &self,
        session_id: &str,
    ) -> BoxFuture<'_, Result<Vec<ReplayStoreBlob>, ReplayStoreError>> {
        self.inner.load(session_id)
    }

    fn commit(
        &self,
        _session_id: &str,
        _blob: ReplayStoreBlob,
    ) -> BoxFuture<'_, Result<(), ReplayStoreError>> {
        Box::pin(async { Err(ReplayStoreError::new(ReplayStoreErrorCode::QuotaExceeded)) })
    }

    fn remove(
        &self,
        session_id: &str,
        generation_ids: &[String],
    ) -> BoxFuture<'_, Result<(), ReplayStoreError>> {
        self.inner.remove(session_id, generation_ids)
    }

    fn delete_session(&self, session_id: &str) -> BoxFuture<'_, Result<(), ReplayStoreError>> {
        self.inner.delete_session(session_id)
    }
}

#[tokio::test]
async fn required_commit_failure_is_emitted_instead_of_completed() {
    let calls = [("call-1", "read", r#"{"path":"a.txt"}"#)];
    let transport = StubTransport::new([StubResponse::Sse(responses_tool_body(
        "response-1",
        "required-state",
        &calls,
    ))]);
    let adapter = response_adapter(transport, Arc::new(CommitFailureStore::new()), "stub-model");
    let events = collect(
        adapter
            .start(reasoning_snapshot(
                "turn-1",
                1,
                None,
                vec![user("use the tool")],
                vec![support::read_tool_definition()],
            ))
            .await
            .expect("call starts"),
    )
    .await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ModelEvent::Completed { .. })))
    );
    let error = events
        .last()
        .expect("terminal event")
        .as_ref()
        .expect_err("required commit fails");
    assert!(error.message().contains("could not be persisted"));
    assert!(!error.message().contains("required-state"));
}

#[tokio::test]
async fn optional_commit_failure_degrades_and_still_completes() {
    let adapter = response_adapter(
        StubTransport::new([StubResponse::Sse(MINIMAL_RESPONSE.to_vec())]),
        Arc::new(CommitFailureStore::new()),
        "stub-model",
    );
    let events = collect_ok(
        adapter
            .start(reasoning_snapshot(
                "turn-1",
                1,
                None,
                vec![user("answer")],
                Vec::new(),
            ))
            .await
            .expect("call starts"),
    )
    .await;
    assert!(matches!(events.last(), Some(ModelEvent::Completed { .. })));
}

#[tokio::test]
async fn file_store_is_idempotent_quota_bounded_and_session_scoped() {
    let root = TempRoot::new("phase2-store");
    let store = FileModelReplayStore::with_policy(
        &root.path,
        ReplayStorePolicy {
            max_session_bytes: 6,
            ..ReplayStorePolicy::default()
        },
    )
    .expect("open replay store");
    store
        .commit("session-a", ReplayStoreBlob::new("one", b"abc".to_vec()))
        .await
        .expect("first commit");
    store
        .commit("session-a", ReplayStoreBlob::new("one", b"abc".to_vec()))
        .await
        .expect("idempotent commit");
    assert_eq!(store.load("session-a").await.expect("load").len(), 1);
    assert_eq!(
        store
            .commit("session-a", ReplayStoreBlob::new("one", b"abd".to_vec()))
            .await
            .expect_err("same id with different bytes conflicts")
            .code(),
        ReplayStoreErrorCode::Conflict
    );
    assert_eq!(
        store
            .commit("session-a", ReplayStoreBlob::new("two", b"defg".to_vec()))
            .await
            .expect_err("session quota enforced")
            .code(),
        ReplayStoreErrorCode::QuotaExceeded
    );
    assert!(
        store
            .load("session-b")
            .await
            .expect("other session")
            .is_empty()
    );
    store
        .delete_session("session-a")
        .await
        .expect("delete sidecar");
    assert!(
        store
            .load("session-a")
            .await
            .expect("deleted session")
            .is_empty()
    );
}

#[tokio::test]
async fn file_store_serializes_concurrent_writers_and_cleans_temp_files() {
    let root = TempRoot::new("phase2-concurrent");
    let store = Arc::new(FileModelReplayStore::open(&root.path).expect("open replay store"));
    let mut workers = Vec::new();
    for index in 0..8 {
        let store = store.clone();
        workers.push(tokio::spawn(async move {
            store
                .commit(
                    "shared-session",
                    ReplayStoreBlob::new(format!("generation-{index}"), vec![index as u8; 32]),
                )
                .await
        }));
    }
    for worker in workers {
        worker.await.expect("writer task").expect("commit");
    }
    assert_eq!(store.load("shared-session").await.expect("load").len(), 8);

    let replay_dir = root.path.join("s-shared-session").join("model-replay");
    std::fs::write(replay_dir.join(".interrupted.tmp"), b"partial secret")
        .expect("write interrupted temp file");
    assert_eq!(store.load("shared-session").await.expect("reload").len(), 8);
    assert!(!replay_dir.join(".interrupted.tmp").exists());
}

#[tokio::test]
async fn expired_or_orphaned_generations_are_not_restored() {
    let root = TempRoot::new("phase2-retention");
    let calls = [("call-1", "read", r#"{"path":"a.txt"}"#)];
    let policy = ReplayStorePolicy {
        ttl: Duration::ZERO,
        orphan_grace: Duration::ZERO,
        ..ReplayStorePolicy::default()
    };
    let store =
        Arc::new(FileModelReplayStore::with_policy(&root.path, policy).expect("open replay store"));
    let first = response_adapter(
        StubTransport::new([StubResponse::Sse(responses_tool_body(
            "response-1",
            "expired-state",
            &calls,
        ))]),
        store.clone(),
        "stub-model",
    );
    collect_ok(
        first
            .start(reasoning_snapshot(
                "turn-1",
                1,
                None,
                vec![user("use the tool")],
                vec![support::read_tool_definition()],
            ))
            .await
            .expect("first call starts"),
    )
    .await;
    assert_eq!(
        store.load("session-1").await.expect("raw store load").len(),
        1
    );

    let transport = StubTransport::new([StubResponse::Sse(MINIMAL_RESPONSE.to_vec())]);
    let next = response_adapter(transport.clone(), store, "stub-model");
    collect_ok(
        next.start(reasoning_snapshot(
            "turn-2",
            1,
            None,
            tool_history(&calls),
            vec![support::read_tool_definition()],
        ))
        .await
        .expect("expired fallback starts"),
    )
    .await;
    let bodies = transport.request_bodies();
    let input = bodies[0]["input"].as_array().expect("Responses input");
    assert!(input.iter().all(|item| item["type"] != "reasoning"));
    assert!(input.iter().any(|item| item["call_id"] == "call-1"));
}

struct RestartIdSource {
    inner: SequentialIdSource,
}

impl IdSource for RestartIdSource {
    fn next_operation_id(&self) -> philo_agent_runtime::OperationId {
        philo_agent_runtime::OperationId::new(format!(
            "restart-{}",
            self.inner.next_operation_id().as_str()
        ))
    }

    fn next_turn_id(&self) -> philo_agent_runtime::TurnId {
        philo_agent_runtime::TurnId::new(format!("restart-{}", self.inner.next_turn_id().as_str()))
    }
}

fn replay_runtime_config() -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "phase 2 test".to_owned(),
        model_target: "stub-model".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds: 1,
        max_parallel_tool_calls: 1,
        operation_timeout: None,
        tool_cancel_grace: std::time::Duration::from_millis(300),
        compaction: Default::default(),
        recovery: Default::default(),
    }
}

fn replay_tools(workspace: &std::path::Path) -> Arc<dyn philo_agent_runtime::ToolPort> {
    Arc::new(
        ToolRegistry::builder()
            .register(ReadTool::definition(), ReadTool::new(workspace))
            .expect("register read tool")
            .build(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn jsonl_session_and_private_replay_sidecar_continue_after_restart() {
    let root = TempRoot::new("phase2-e2e");
    let workspace = root.path.join("workspace");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    std::fs::write(workspace.join("a.txt"), "alpha").expect("write tool fixture");
    let calls = [("call-1", "read", r#"{"path":"a.txt"}"#)];
    let session = SessionId::new("phase2-session");

    {
        let sessions = Arc::new(JsonlSessionStore::open(&root.path).expect("open session store"));
        let replay = Arc::new(FileModelReplayStore::open(&root.path).expect("open replay store"));
        let transport = StubTransport::new([
            StubResponse::Sse(responses_tool_body(
                "response-1",
                "opaque-e2e-state",
                &calls,
            )),
            StubResponse::Sse(MINIMAL_RESPONSE.to_vec()),
        ]);
        let generation = test_generation(
            response_adapter(transport, replay, "stub-model"),
            replay_tools(&workspace),
            replay_runtime_config(),
        );
        let (handle, mut sub) = start_runtime(sessions, Arc::new(SequentialIdSource::new()));
        let operation_id = submit_prompt(
            &handle,
            session.clone(),
            UserMessage::new("read a.txt"),
            generation,
        )
        .await;
        let (_events, outcome) = drain_until_settled(&mut sub, &operation_id).await;
        assert!(matches!(outcome, OperationOutcome::Succeeded { .. }));
        handle
            .shutdown(
                ShutdownMode::Drain,
                Instant::now() + Duration::from_secs(30),
            )
            .await
            .expect("shutdown");
    }

    let sessions = Arc::new(JsonlSessionStore::open(&root.path).expect("reopen session store"));
    let replay = Arc::new(FileModelReplayStore::open(&root.path).expect("reopen replay store"));
    let transport = StubTransport::new([StubResponse::Sse(MINIMAL_RESPONSE.to_vec())]);
    let generation = test_generation(
        response_adapter(transport.clone(), replay, "stub-model"),
        replay_tools(&workspace),
        replay_runtime_config(),
    );
    let (handle, mut sub) = start_runtime(
        sessions,
        Arc::new(RestartIdSource {
            inner: SequentialIdSource::new(),
        }),
    );
    let operation_id =
        submit_prompt(&handle, session, UserMessage::new("continue"), generation).await;
    let (_events, outcome) = drain_until_settled(&mut sub, &operation_id).await;
    assert!(matches!(outcome, OperationOutcome::Succeeded { .. }));
    handle
        .shutdown(
            ShutdownMode::Drain,
            Instant::now() + Duration::from_secs(30),
        )
        .await
        .expect("shutdown");

    let bodies = transport.request_bodies();
    let input = bodies[0]["input"].as_array().expect("Responses input");
    assert_eq!(input[1]["type"], "reasoning");
    assert_eq!(input[1]["encrypted_content"], "opaque-e2e-state");
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[3]["type"], "function_call_output");
    assert_eq!(input[4]["type"], "message");
    assert_eq!(input[4]["role"], "assistant");

    let log = std::fs::read_to_string(root.path.join("s-phase2-session").join("log.jsonl"))
        .expect("read ordinary session log");
    assert!(!log.contains("opaque-e2e-state"));
    assert!(!log.contains("response-1"));
    let replay_records = std::fs::read_dir(root.path.join("s-phase2-session").join("model-replay"))
        .expect("read private sidecar")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("replay"))
        .count();
    assert!(replay_records >= 2, "tool and message generations persist");
}

#[cfg(unix)]
#[tokio::test]
async fn file_store_uses_owner_only_unix_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = TempRoot::new("phase2-permissions");
    let store = FileModelReplayStore::open(&root.path).expect("open replay store");
    store
        .commit(
            "session",
            ReplayStoreBlob::new("generation", b"secret".to_vec()),
        )
        .await
        .expect("commit");
    let replay_dir = root.path.join("s-session").join("model-replay");
    let record = std::fs::read_dir(&replay_dir)
        .expect("read sidecar")
        .map(|entry| entry.expect("sidecar entry").path())
        .find(|path| path.extension().and_then(|value| value.to_str()) == Some("replay"))
        .expect("replay record");
    assert_eq!(
        std::fs::metadata(replay_dir)
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(record)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}
