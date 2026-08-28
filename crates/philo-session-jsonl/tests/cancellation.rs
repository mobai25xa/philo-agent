//! JSONL-002: cancelled outcomes on schema v2, with required `reason`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use philo_session::{
    CancelReason, ContextMessage, MemorySessionStore, OperationId, OperationOutcome,
    SessionAssistantBlock, SessionEntryKind, SessionRevision, SessionStore, SessionToolCall,
    SessionToolResult, SessionTransaction, SessionUserPart, ToolBatchId, ToolCallId,
    ToolResultOutcome, TurnId, TurnOutcome,
};
use philo_session_jsonl::{JsonlOpenError, JsonlSessionStore};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-session-jsonl-m6-{}-{}",
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

fn session_id() -> philo_session::SessionId {
    philo_session::SessionId::new("golden")
}

fn log_path(root: &TempRoot) -> PathBuf {
    root.path.join("s-golden").join("log.jsonl")
}

fn start_transaction(revision: u64) -> SessionTransaction {
    SessionTransaction::linear(
        session_id(),
        SessionRevision::new(revision),
        vec![
            SessionEntryKind::OperationStarted {
                operation_id: OperationId::new("op-1"),
            },
            SessionEntryKind::TurnStarted {
                operation_id: OperationId::new("op-1"),
                turn_id: TurnId::new("turn-1"),
            },
            SessionEntryKind::UserMessage {
                turn_id: TurnId::new("turn-1"),
                parts: SessionUserPart::text_parts("read two files"),
            },
        ],
    )
}

fn batch_transaction(revision: u64) -> SessionTransaction {
    SessionTransaction::linear(
        session_id(),
        SessionRevision::new(revision),
        vec![SessionEntryKind::AssistantToolCallBatch {
            turn_id: TurnId::new("turn-1"),
            model_call_id: "model-call-1".to_owned(),
            tool_batch_id: ToolBatchId::new("batch-1"),
            blocks: vec![
                SessionAssistantBlock::ToolCall(SessionToolCall::new(
                    ToolCallId::new("call-1"),
                    "read",
                    r#"{"path":"a.txt"}"#,
                )),
                SessionAssistantBlock::ToolCall(SessionToolCall::new(
                    ToolCallId::new("call-2"),
                    "read",
                    r#"{"path":"b.txt"}"#,
                )),
            ],
        }],
    )
}

/// Partial execution: call-1 finished for real, call-2 never ran.
fn cancel_transaction(revision: u64) -> SessionTransaction {
    SessionTransaction::linear(
        session_id(),
        SessionRevision::new(revision),
        vec![
            SessionEntryKind::ToolResult {
                turn_id: TurnId::new("turn-1"),
                tool_batch_id: ToolBatchId::new("batch-1"),
                result: SessionToolResult::success(ToolCallId::new("call-1"), "ok"),
            },
            SessionEntryKind::ToolResult {
                turn_id: TurnId::new("turn-1"),
                tool_batch_id: ToolBatchId::new("batch-1"),
                result: SessionToolResult::cancelled(ToolCallId::new("call-2")),
            },
            SessionEntryKind::TurnTerminated {
                turn_id: TurnId::new("turn-1"),
                outcome: TurnOutcome::Cancelled {
                    reason: CancelReason::User,
                },
            },
            SessionEntryKind::OperationSettled {
                operation_id: OperationId::new("op-1"),
                outcome: OperationOutcome::Cancelled {
                    reason: CancelReason::User,
                },
                usage: None,
            },
        ],
    )
}

async fn commit_cancelled_turn(store: &dyn SessionStore) {
    for transaction in [
        start_transaction(0),
        batch_transaction(1),
        cancel_transaction(2),
    ] {
        store
            .commit(transaction)
            .await
            .expect("valid transaction commits");
    }
}

// --- Golden format for cancellation lines ------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn golden_cancellation_transaction_uses_schema_v2() {
    let root = TempRoot::new();
    let store = JsonlSessionStore::open(&root.path).expect("open");
    commit_cancelled_turn(&store).await;
    drop(store);

    let log = std::fs::read_to_string(log_path(&root)).expect("read log");
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(
        lines.iter().all(|line| line.starts_with(r#"{"v":2,"#)),
        "cancellation writes schema v2"
    );
    assert_eq!(
        lines[2],
        r#"{"v":2,"revision":3,"entries":[{"id":"golden:entry:5","parent":"golden:entry:4","kind":{"type":"tool_result","turn_id":"turn-1","tool_batch_id":"batch-1","result":{"call_id":"call-1","status":"success","content":"ok"}}},{"id":"golden:entry:6","parent":"golden:entry:5","kind":{"type":"tool_result","turn_id":"turn-1","tool_batch_id":"batch-1","result":{"call_id":"call-2","status":"cancelled"}}},{"id":"golden:entry:7","parent":"golden:entry:6","kind":{"type":"turn_terminated","turn_id":"turn-1","outcome":"cancelled","reason":"user"}},{"id":"golden:entry:8","parent":"golden:entry:7","kind":{"type":"operation_settled","operation_id":"op-1","outcome":"cancelled","reason":"user"}}]}"#
    );
}

// --- v1 files are refused ------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn schema_v1_file_is_unsupported() {
    let root = TempRoot::new();
    let dir = root.path.join("s-golden");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("log.jsonl"),
        concat!(
            r#"{"v":1,"revision":1,"entries":[{"id":"golden:entry:1","kind":{"type":"operation_started","operation_id":"op-1"}},{"id":"golden:entry:2","parent":"golden:entry:1","kind":{"type":"turn_started","operation_id":"op-1","turn_id":"turn-1"}},{"id":"golden:entry:3","parent":"golden:entry:2","kind":{"type":"user_message","turn_id":"turn-1","content":"hi"}}]}"#,
            "\n",
            r#"{"v":1,"revision":2,"entries":[{"id":"golden:entry:4","parent":"golden:entry:3","kind":{"type":"assistant_message","turn_id":"turn-1","content":"hello"}},{"id":"golden:entry:5","parent":"golden:entry:4","kind":{"type":"turn_terminated","turn_id":"turn-1","outcome":"succeeded"}},{"id":"golden:entry:6","parent":"golden:entry:5","kind":{"type":"operation_settled","operation_id":"op-1","outcome":"succeeded"}}]}"#,
            "\n",
        ),
    )
    .expect("write v1 file");

    let store = JsonlSessionStore::open(&root.path).expect("open");
    let error = store.recover_session(&session_id()).expect_err("refused");
    assert!(matches!(
        error,
        JsonlOpenError::UnsupportedSchema { found: 1 }
    ));
}

// --- Roundtrip: write, reopen, project ----------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_roundtrips_across_a_restart() {
    let root = TempRoot::new();
    {
        let store = JsonlSessionStore::open(&root.path).expect("open");
        commit_cancelled_turn(&store).await;
    }
    let reopened = JsonlSessionStore::open(&root.path).expect("re-open");
    let view = reopened.context_view(&session_id()).await.expect("view");
    assert_eq!(view.revision(), SessionRevision::new(3));
    let messages = view.messages();
    assert_eq!(
        messages.len(),
        4,
        "user, calls, real result, cancelled mark"
    );
    assert!(matches!(
        &messages[2],
        ContextMessage::ToolResult {
            tool_call_id,
            outcome: ToolResultOutcome::Success { content },
        } if tool_call_id.as_str() == "call-1" && content == "ok"
    ));
    assert!(matches!(
        &messages[3],
        ContextMessage::ToolResult {
            tool_call_id,
            outcome: ToolResultOutcome::Cancelled,
        } if tool_call_id.as_str() == "call-2"
    ));

    // The session continues normally after the cancelled turn.
    let next = SessionTransaction::linear(
        session_id(),
        SessionRevision::new(3),
        vec![
            SessionEntryKind::OperationStarted {
                operation_id: OperationId::new("op-2"),
            },
            SessionEntryKind::TurnStarted {
                operation_id: OperationId::new("op-2"),
                turn_id: TurnId::new("turn-2"),
            },
            SessionEntryKind::UserMessage {
                turn_id: TurnId::new("turn-2"),
                parts: SessionUserPart::text_parts("continue"),
            },
        ],
    );
    let commit = reopened
        .commit(next)
        .await
        .expect("commit after cancellation");
    assert_eq!(commit.revision(), SessionRevision::new(4));
    assert_eq!(commit.entries()[0].id().as_str(), "golden:entry:9");
}

// --- Backend parity extended to cancellation -----------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn jsonl_and_memory_backends_agree_on_cancellation() {
    let root = TempRoot::new();
    let jsonl = JsonlSessionStore::open(&root.path).expect("open");
    let memory = MemorySessionStore::new();

    for transaction in [
        start_transaction(0),
        batch_transaction(1),
        cancel_transaction(2),
    ] {
        let disk = jsonl
            .commit(transaction.clone())
            .await
            .expect("jsonl commit");
        let ram = memory.commit(transaction).await.expect("memory commit");
        assert_eq!(disk.revision(), ram.revision());
        assert_eq!(disk.entries(), ram.entries(), "same ids, parents, kinds");
        assert_eq!(disk.current_leaf(), ram.current_leaf());
    }
    assert_eq!(
        jsonl.context_view(&session_id()).await.expect("jsonl view"),
        memory
            .context_view(&session_id())
            .await
            .expect("memory view")
    );

    // The same illegal cancellation shape is rejected identically: a plain
    // result commit must not carry cancelled marks.
    let root2 = TempRoot::new();
    let jsonl2 = JsonlSessionStore::open(&root2.path).expect("open");
    let memory2 = MemorySessionStore::new();
    for transaction in [start_transaction(0), batch_transaction(1)] {
        jsonl2
            .commit(transaction.clone())
            .await
            .expect("jsonl commit");
        memory2.commit(transaction).await.expect("memory commit");
    }
    let illegal = SessionTransaction::linear(
        session_id(),
        SessionRevision::new(2),
        vec![
            SessionEntryKind::ToolResult {
                turn_id: TurnId::new("turn-1"),
                tool_batch_id: ToolBatchId::new("batch-1"),
                result: SessionToolResult::success(ToolCallId::new("call-1"), "ok"),
            },
            SessionEntryKind::ToolResult {
                turn_id: TurnId::new("turn-1"),
                tool_batch_id: ToolBatchId::new("batch-1"),
                result: SessionToolResult::cancelled(ToolCallId::new("call-2")),
            },
        ],
    );
    let jsonl_error = jsonl2.commit(illegal.clone()).await.expect_err("rejected");
    let memory_error = memory2.commit(illegal).await.expect_err("rejected");
    assert_eq!(jsonl_error, memory_error);
}
