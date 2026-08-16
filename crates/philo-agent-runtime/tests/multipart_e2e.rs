//! INTEGRATION-008: image-bearing sessions end to end over the JSONL durable
//! backend — multi-round fidelity, artifact persistence, restart replay, and
//! crash-semantics propagation (M8-001 / M8-002 / M8-003 / M8-004 / M8-008).

mod support;

use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll, Waker};

use philo_agent_runtime::{
    AgentRuntime, GenerationConfig, ModelMessage, OperationOutcome, RuntimeConfig,
    SequentialIdSource, SessionId, ToolDefinition, UserMessage, UserPart,
};
use philo_session::{ContextMessage, SessionStore, SessionUserPart};
use philo_session_jsonl::JsonlSessionStore;
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
    }
}

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-m8-e2e-{}-{}",
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

fn config(max_tool_rounds: u32) -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "sys".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds,
        max_parallel_tool_calls: 1,
        operation_timeout: None,
        compaction: Default::default(),
    }
}

fn runtime(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<FakeTool>,
    max_tool_rounds: u32,
) -> AgentRuntime {
    AgentRuntime::with_tools(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(max_tool_rounds),
        tools,
    )
}

/// Simulates the restarted process's unique-ID responsibility.
struct RestartIdSource {
    inner: SequentialIdSource,
}

impl philo_agent_runtime::IdSource for RestartIdSource {
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

/// Image bytes `abc`: sha256 pins the artifact file name.
const IMAGE_BYTES: &[u8] = b"abc";
const IMAGE_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

fn png_message() -> UserMessage {
    UserMessage::from_parts(vec![
        UserPart::Text("what is in this picture?".to_owned()),
        UserPart::Image {
            media_type: "image/png".to_owned(),
            bytes: IMAGE_BYTES.to_vec(),
        },
    ])
    .expect("text+image is valid")
}

fn model_parts() -> Vec<UserPart> {
    vec![
        UserPart::Text("what is in this picture?".to_owned()),
        UserPart::Image {
            media_type: "image/png".to_owned(),
            bytes: IMAGE_BYTES.to_vec(),
        },
    ]
}

fn session_dir(root: &TempRoot, encoded: &str) -> PathBuf {
    root.path.join(encoded)
}

/// M8-001 + M8-002 + M8-008(v=1): an image-bearing turn completes a tool loop
/// on the JSONL backend; the artifact is durable and content-addressed, the
/// log lines stay small references under envelope v1.
#[test]
fn image_turn_completes_a_tool_loop_on_jsonl() {
    let root = TempRoot::new();
    let sessions = Arc::new(JsonlSessionStore::open(&root.path).expect("open store"));
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::text(&["a cat"]),
    ]));
    let tools = Arc::new(FakeTool::new(
        [ToolDefinition::simple(
            "echo",
            "echo",
            philo_agent_runtime::EffectClass::ReadOnly,
        )],
        [FakeToolResult::success("ok")],
    ));

    let handle = block_on(
        runtime(model.clone(), sessions.clone(), tools, 1)
            .prompt(SessionId::new("m8-001"), png_message()),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "a cat"
    ));

    // Both model calls saw the image parts byte-for-byte (M8-001).
    for call in model.calls() {
        assert_eq!(
            call.messages[1],
            ModelMessage::User {
                parts: model_parts()
            }
        );
    }

    // The artifact is durable under its content hash; the log carries only
    // references and keeps envelope v1 (M8-002, M8-008).
    let dir = session_dir(&root, "s-m8-001");
    assert_eq!(
        std::fs::read(dir.join("artifacts").join(IMAGE_SHA256)).expect("artifact"),
        IMAGE_BYTES
    );
    let log = std::fs::read_to_string(dir.join("log.jsonl")).expect("log");
    for line in log.lines() {
        assert!(line.starts_with(r#"{"v":1,"#), "envelope v stays 1");
    }
    assert!(
        log.contains(&format!(r#""artifact":"{IMAGE_SHA256}""#)),
        "the user message row references the artifact"
    );
}

/// M8-003: after a restart the image turn's context rebuilds byte-for-byte
/// and the next turn's model call snapshot carries the image parts.
#[test]
fn restart_replays_image_context_into_the_next_snapshot() {
    let root = TempRoot::new();
    let session = SessionId::new("m8-003");
    {
        let sessions = Arc::new(JsonlSessionStore::open(&root.path).expect("open store"));
        let model = Arc::new(FakeModel::succeeds(&["a cat"]));
        let handle = block_on(
            runtime(model, sessions, Arc::new(FakeTool::new([], [])), 0)
                .prompt(session.clone(), png_message()),
        )
        .unwrap();
        assert!(matches!(
            block_on(handle.wait()),
            OperationOutcome::Succeeded { .. }
        ));
        // The first "process" ends here; its store instance drops with it.
    }

    let reopened = Arc::new(JsonlSessionStore::open(&root.path).expect("re-open store"));
    let view =
        block_on(reopened.context_view(&philo_session::SessionId::new("m8-003"))).expect("view");
    assert_eq!(
        view.messages()[0],
        ContextMessage::User {
            parts: vec![
                SessionUserPart::Text("what is in this picture?".to_owned()),
                SessionUserPart::Image {
                    media_type: "image/png".to_owned(),
                    bytes: IMAGE_BYTES.to_vec(),
                },
            ]
        },
        "the image context rebuilds byte-for-byte from the artifact"
    );

    let model = Arc::new(FakeModel::succeeds(&["still a cat"]));
    let restarted = AgentRuntime::with_tools(
        model.clone(),
        reopened,
        Arc::new(RestartIdSource {
            inner: SequentialIdSource::new(),
        }),
        config(0),
        Arc::new(FakeTool::new([], [])),
    );
    let next = block_on(restarted.prompt(session, UserMessage::new("and now?"))).unwrap();
    assert!(matches!(
        block_on(next.wait()),
        OperationOutcome::Succeeded { .. }
    ));

    // The replayed history enters the next model call with the image intact.
    let calls = model.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].messages[1],
        ModelMessage::User {
            parts: model_parts()
        }
    );
    assert_eq!(
        calls[0].messages[3],
        ModelMessage::User {
            parts: vec![UserPart::Text("and now?".to_owned())]
        }
    );
}

/// M8-004 across modules: a missing referenced artifact surfaces through the
/// runtime as a store failure on the next prompt — never silently skipped.
#[test]
fn missing_artifact_fails_the_next_prompt_as_store_unavailable() {
    let root = TempRoot::new();
    let session = SessionId::new("m8-004");
    {
        let sessions = Arc::new(JsonlSessionStore::open(&root.path).expect("open store"));
        let model = Arc::new(FakeModel::succeeds(&["a cat"]));
        let handle = block_on(
            runtime(model, sessions, Arc::new(FakeTool::new([], [])), 0)
                .prompt(session.clone(), png_message()),
        )
        .unwrap();
        assert!(matches!(
            block_on(handle.wait()),
            OperationOutcome::Succeeded { .. }
        ));
    }
    // Crash-adjacent damage: the referenced artifact disappears.
    std::fs::remove_file(
        session_dir(&root, "s-m8-004")
            .join("artifacts")
            .join(IMAGE_SHA256),
    )
    .expect("delete artifact");

    let reopened = Arc::new(JsonlSessionStore::open(&root.path).expect("open store"));
    let model = Arc::new(FakeModel::succeeds(&["unused"]));
    let restarted = AgentRuntime::with_tools(
        model.clone(),
        reopened,
        Arc::new(RestartIdSource {
            inner: SequentialIdSource::new(),
        }),
        config(0),
        Arc::new(FakeTool::new([], [])),
    );
    let handle = block_on(restarted.prompt(session, UserMessage::new("continue"))).unwrap();
    let OperationOutcome::Failed { failure, .. } = block_on(handle.wait()) else {
        panic!("a corrupt session must fail the prompt");
    };
    assert_eq!(
        failure.kind(),
        philo_agent_runtime::AgentFailureKind::Persistence
    );
    assert!(model.calls().is_empty(), "no model call over corrupt state");
}
