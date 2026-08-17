//! INTEGRATION-011: crash-remnant sealing over the durable JSONL backend,
//! placeholder mapping with byte-identical durable history, the timeout
//! reason on disk, and legacy continuation (M11-001/004/005/006).

mod support;

use std::future::Future;
use std::path::PathBuf;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use philo_agent_runtime::{
    AgentEvent, AgentRuntime, GenerationConfig, ModelMessage, ModelToolResultOutcome,
    OperationHandle, OperationOutcome, OperationPhase, RunningToolBatchPhase, RuntimeConfig,
    SequentialIdSource, SessionId, ToolDefinition, UserMessage,
};
use philo_session::{
    SessionAssistantBlock, SessionEntryKind, SessionRevision, SessionStore, SessionToolCall,
    SessionTransaction, SessionUserPart,
};
use philo_session_jsonl::JsonlSessionStore;
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};
use support::gate::Gate;

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn poll_once<F: Future + ?Sized>(future: &mut Pin<Box<F>>) -> Poll<F::Output> {
    let mut context = Context::from_waker(Waker::noop());
    future.as_mut().poll(&mut context)
}

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-m11-e2e-{}-{}",
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

fn config(max_tool_rounds: u32, operation_timeout: Option<Duration>) -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "sys".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds,
        max_parallel_tool_calls: 1,
        operation_timeout,
        tool_cancel_grace: std::time::Duration::from_millis(300),
        compaction: Default::default(),
    }
}

fn runtime(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<FakeTool>,
    max_tool_rounds: u32,
    operation_timeout: Option<Duration>,
) -> AgentRuntime {
    AgentRuntime::with_tools(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(max_tool_rounds, operation_timeout),
        tools,
    )
}

fn sid() -> SessionId {
    SessionId::new("s")
}

fn stored_sid() -> philo_session::SessionId {
    philo_session::SessionId::new("s")
}

fn log_path(root: &TempRoot) -> PathBuf {
    root.path.join("s-s").join("log.jsonl")
}

fn read_log(root: &TempRoot) -> String {
    std::fs::read_to_string(log_path(root)).expect("read session log")
}

fn collect_events(mut handle: OperationHandle) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = block_on(handle.next_event()) {
        events.push(event);
    }
    events
}

/// Persists the crash window between B_k and C_k: start facts and a
/// two-call batch, no results, no terminal facts.
fn persist_crash_remnant(store: &JsonlSessionStore) {
    block_on(store.commit(SessionTransaction::linear(
        stored_sid(),
        SessionRevision::new(0),
        vec![
            SessionEntryKind::OperationStarted {
                operation_id: philo_session::OperationId::new("stale-op"),
            },
            SessionEntryKind::TurnStarted {
                operation_id: philo_session::OperationId::new("stale-op"),
                turn_id: philo_session::TurnId::new("stale-turn"),
            },
            SessionEntryKind::UserMessage {
                turn_id: philo_session::TurnId::new("stale-turn"),
                parts: SessionUserPart::text_parts("edit files"),
            },
        ],
    )))
    .expect("remnant start commits");
    block_on(store.commit(SessionTransaction::linear(
        stored_sid(),
        SessionRevision::new(1),
        vec![SessionEntryKind::AssistantToolCallBatch {
            turn_id: philo_session::TurnId::new("stale-turn"),
            model_call_id: "stale-model-call".to_owned(),
            tool_batch_id: philo_session::ToolBatchId::new("stale-batch"),
            blocks: vec![
                SessionAssistantBlock::ToolCall(SessionToolCall::new(
                    philo_session::ToolCallId::new("stale-call-1"),
                    "write",
                    r#"{"path":"a.txt"}"#,
                )),
                SessionAssistantBlock::ToolCall(SessionToolCall::new(
                    philo_session::ToolCallId::new("stale-call-2"),
                    "shell",
                    r#"{"command":"cargo test"}"#,
                )),
            ],
        }],
    )))
    .expect("remnant batch commits");
}

fn echo() -> ToolDefinition {
    ToolDefinition::simple("echo", "echo", philo_agent_runtime::EffectClass::ReadOnly)
}

// ------------------------------------------------- M11-001 崩溃残留端到端

#[test]
fn crash_remnant_seals_on_disk_and_the_session_continues() {
    let root = TempRoot::new();
    {
        let store = JsonlSessionStore::open(&root.path).expect("open");
        persist_crash_remnant(&store);
        // The "crash": the store drops with the batch unresolved.
    }

    let reopened = Arc::new(JsonlSessionStore::open(&root.path).expect("re-open"));
    let model = Arc::new(FakeModel::succeeds(&["continuing"]));
    let agent = runtime(
        model.clone(),
        reopened.clone(),
        Arc::new(FakeTool::new([], [])),
        0,
        None,
    );
    let handle = block_on(agent.prompt(sid(), UserMessage::new("continue"))).unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "continuing"
    ));
    let events = collect_events(handle);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::PriorTurnSealed { .. })),
        "the runtime reports the seal"
    );

    // Line-level audit: the seal is one atomic line completing every call
    // with interrupted marks and both abandoned terminal facts.
    let log = read_log(&root);
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 5, "remnant(2) + seal(1) + new turn(2)");
    let seal = lines[2];
    assert_eq!(seal.matches(r#""status":"interrupted""#).count(), 2);
    assert!(seal.contains(r#""tool_batch_id":"stale-batch""#));
    assert_eq!(seal.matches(r#""reason":"abandoned""#).count(), 2);
    assert!(seal.contains(r#""type":"turn_terminated","turn_id":"stale-turn""#));
    assert!(seal.contains(r#""type":"operation_settled","operation_id":"stale-op""#));

    // The next model request pairs every dangling call: the exact shape
    // that providers previously rejected with a 400.
    let request = &model.calls()[0];
    let interrupted = request
        .messages
        .iter()
        .filter(|message| {
            matches!(
                message,
                ModelMessage::ToolResult {
                    outcome: ModelToolResultOutcome::Interrupted,
                    ..
                }
            )
        })
        .count();
    assert_eq!(interrupted, 2, "both stale calls travel fully paired");
}

// ------------------------------------------------- M11-004 占位零改动

#[test]
fn placeholders_add_nothing_to_the_durable_log() {
    let root = TempRoot::new();
    let store = Arc::new(JsonlSessionStore::open(&root.path).expect("open"));
    persist_crash_remnant(store.as_ref());
    // Terminate the stale turn the failure way: the batch stays dangling
    // but the turn is closed, so sealing never touches it.
    block_on(store.commit(SessionTransaction::linear(
        stored_sid(),
        SessionRevision::new(2),
        vec![
            SessionEntryKind::TurnFailure {
                turn_id: philo_session::TurnId::new("stale-turn"),
                failure: philo_session::TurnFailure::new(
                    philo_session::TurnFailureKind::ModelCall,
                    "provider offline",
                ),
            },
            SessionEntryKind::TurnTerminated {
                turn_id: philo_session::TurnId::new("stale-turn"),
                outcome: philo_session::TurnOutcome::Failed,
            },
            SessionEntryKind::OperationSettled {
                operation_id: philo_session::OperationId::new("stale-op"),
                outcome: philo_session::OperationOutcome::Failed,
            },
        ],
    )))
    .expect("failure transaction commits");
    let log_before = read_log(&root);

    let model = Arc::new(FakeModel::succeeds(&["ok"]));
    let agent = runtime(
        model.clone(),
        store,
        Arc::new(FakeTool::new([], [])),
        0,
        None,
    );
    let handle = block_on(agent.prompt(sid(), UserMessage::new("continue"))).unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { .. }
    ));
    let events = collect_events(handle);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::PriorTurnSealed { .. })),
        "terminated turns are never sealed"
    );

    // The request carries placeholders...
    let placeholders = model.calls()[0]
        .messages
        .iter()
        .filter(|message| {
            matches!(
                message,
                ModelMessage::ToolResult {
                    outcome: ModelToolResultOutcome::Interrupted,
                    ..
                }
            )
        })
        .count();
    assert_eq!(placeholders, 2);

    // ...while the durable history is byte-for-byte what it was, plus only
    // the new turn's two transactions.
    let log_after = read_log(&root);
    assert!(
        log_after.starts_with(&log_before),
        "existing lines stay byte-identical"
    );
    assert_eq!(
        log_after.lines().count(),
        log_before.lines().count() + 2,
        "only the new turn appended lines"
    );
    assert!(
        !log_after.contains(r#""status":"interrupted""#),
        "no placeholder ever lands on disk"
    );
}

// ------------------------------------------------- M11-005 超时 reason 落盘

#[test]
fn timeout_cancellation_lands_on_disk_with_its_reason() {
    let root = TempRoot::new();
    let store = Arc::new(JsonlSessionStore::open(&root.path).expect("open"));
    let gate = Gate::new();
    let model = Arc::new(FakeModel::new([ModelScript::tool_calls(&[
        (0, "call-1", "echo", "{}"),
        (1, "call-2", "echo", "{}"),
    ])]));
    let tools = Arc::new(FakeTool::new(
        [echo()],
        [
            FakeToolResult::gated_success(&gate, "one"),
            FakeToolResult::success("never"),
        ],
    ));
    // Leave enough headroom for the JSONL tests running in parallel to reach
    // the gated tool before the timeout window starts testing cancellation.
    let agent = runtime(model, store, tools, 2, Some(Duration::from_millis(250)));

    let handle = block_on(agent.prompt(sid(), UserMessage::new("go"))).unwrap();
    let mut wait = Box::pin(handle.wait());
    loop {
        assert!(poll_once(&mut wait).is_pending());
        if handle.phase()
            == OperationPhase::RunningToolBatch(RunningToolBatchPhase::Executing {
                in_flight: 1,
                completed: 0,
            })
        {
            break;
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    gate.release();
    assert!(matches!(block_on(&mut wait), OperationOutcome::Cancelled));
    drop(wait);
    drop(handle);

    let log = read_log(&root);
    let last = log.lines().last().expect("cancellation line");
    assert!(last.contains(r#""call_id":"call-1","status":"success","content":"one""#));
    assert!(last.contains(r#""call_id":"call-2","status":"cancelled""#));
    assert_eq!(last.matches(r#""reason":"timeout""#).count(), 2);
}

// ------------------------------------------------- M11-006 legacy 延续

#[test]
fn legacy_cancelled_session_continues_after_upgrade() {
    let root = TempRoot::new();
    // A previously cancelled session on schema v2 (jsonl no longer opens v1).
    let dir = root.path.join("s-s");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("log.jsonl"),
        concat!(
            r#"{"v":2,"revision":1,"entries":[{"id":"s:entry:1","kind":{"type":"operation_started","operation_id":"op-legacy"}},{"id":"s:entry:2","parent":"s:entry:1","kind":{"type":"turn_started","operation_id":"op-legacy","turn_id":"turn-legacy"}},{"id":"s:entry:3","parent":"s:entry:2","kind":{"type":"user_message","turn_id":"turn-legacy","parts":[{"type":"text","text":"old prompt"}]}}]}"#,
            "\n",
            r#"{"v":2,"revision":2,"entries":[{"id":"s:entry:4","parent":"s:entry:3","kind":{"type":"turn_terminated","turn_id":"turn-legacy","outcome":"cancelled","reason":"user"}},{"id":"s:entry:5","parent":"s:entry:4","kind":{"type":"operation_settled","operation_id":"op-legacy","outcome":"cancelled","reason":"user"}}]}"#,
            "\n",
        ),
    )
    .expect("write cancelled session");

    let store = Arc::new(JsonlSessionStore::open(&root.path).expect("open"));
    let model = Arc::new(FakeModel::succeeds(&["hello again"]));
    let agent = runtime(model, store, Arc::new(FakeTool::new([], [])), 0, None);
    let handle = block_on(agent.prompt(sid(), UserMessage::new("continue"))).unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "hello again"
    ));
    let events = collect_events(handle);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::PriorTurnSealed { .. })),
        "the legacy turn is terminated: nothing to seal"
    );

    let log = read_log(&root);
    assert_eq!(log.lines().count(), 4, "legacy(2) + new turn(2)");
}
