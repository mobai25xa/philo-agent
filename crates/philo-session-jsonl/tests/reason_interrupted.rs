//! JSONL-005: cancel-reason and `interrupted` serialization on schema v2,
//! and the crash-remnant construction shape used by integration tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use philo_session::{
    CancelReason, MemorySessionStore, OperationId, OperationOutcome, SessionAssistantBlock,
    SessionEntryKind, SessionRevision, SessionStore, SessionToolCall, SessionToolResult,
    SessionTransaction, SessionUserPart, ToolBatchId, ToolCallId, TurnId, TurnOutcome,
};
use philo_session_jsonl::{JsonlOpenError, JsonlSessionStore};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-session-jsonl-m11-{}-{}",
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
                parts: SessionUserPart::text_parts("edit two files"),
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
                    "write",
                    r#"{"path":"a.txt"}"#,
                )),
                SessionAssistantBlock::ToolCall(SessionToolCall::new(
                    ToolCallId::new("call-2"),
                    "shell",
                    r#"{"command":"ls"}"#,
                )),
            ],
        }],
    )
}

fn tool_result(result: SessionToolResult) -> SessionEntryKind {
    SessionEntryKind::ToolResult {
        turn_id: TurnId::new("turn-1"),
        tool_batch_id: ToolBatchId::new("batch-1"),
        result,
    }
}

fn terminal_entries(reason: CancelReason) -> Vec<SessionEntryKind> {
    vec![
        SessionEntryKind::TurnTerminated {
            turn_id: TurnId::new("turn-1"),
            outcome: TurnOutcome::Cancelled { reason },
        },
        SessionEntryKind::OperationSettled {
            operation_id: OperationId::new("op-1"),
            outcome: OperationOutcome::Cancelled { reason },
        },
    ]
}

fn seal_transaction(revision: u64) -> SessionTransaction {
    let mut entries = vec![
        tool_result(SessionToolResult::interrupted(ToolCallId::new("call-1"))),
        tool_result(SessionToolResult::interrupted(ToolCallId::new("call-2"))),
    ];
    entries.extend(terminal_entries(CancelReason::Abandoned));
    SessionTransaction::linear(session_id(), SessionRevision::new(revision), entries)
}

// ---------------------------------------------------------------- golden 形态

#[tokio::test(flavor = "multi_thread")]
async fn golden_seal_transaction_serializes_reason_and_interrupted_at_v2() {
    let root = TempRoot::new();
    let store = JsonlSessionStore::open(&root.path).expect("open");
    for transaction in [
        start_transaction(0),
        batch_transaction(1),
        seal_transaction(2),
    ] {
        store.commit(transaction).await.expect("commit");
    }
    drop(store);

    let log = std::fs::read_to_string(log_path(&root)).expect("read log");
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(
        lines.iter().all(|line| line.starts_with(r#"{"v":2,"#)),
        "reason and interrupted write schema v2"
    );
    assert_eq!(
        lines[2],
        r#"{"v":2,"revision":3,"entries":[{"id":"golden:entry:5","parent":"golden:entry:4","kind":{"type":"tool_result","turn_id":"turn-1","tool_batch_id":"batch-1","result":{"call_id":"call-1","status":"interrupted"}}},{"id":"golden:entry:6","parent":"golden:entry:5","kind":{"type":"tool_result","turn_id":"turn-1","tool_batch_id":"batch-1","result":{"call_id":"call-2","status":"interrupted"}}},{"id":"golden:entry:7","parent":"golden:entry:6","kind":{"type":"turn_terminated","turn_id":"turn-1","outcome":"cancelled","reason":"abandoned"}},{"id":"golden:entry:8","parent":"golden:entry:7","kind":{"type":"operation_settled","operation_id":"op-1","outcome":"cancelled","reason":"abandoned"}}]}"#
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn golden_timeout_cancellation_serializes_its_reason() {
    let root = TempRoot::new();
    let store = JsonlSessionStore::open(&root.path).expect("open");
    let mut entries = vec![
        tool_result(SessionToolResult::success(ToolCallId::new("call-1"), "ok")),
        tool_result(SessionToolResult::cancelled(ToolCallId::new("call-2"))),
    ];
    entries.extend(terminal_entries(CancelReason::Timeout));
    for transaction in [
        start_transaction(0),
        batch_transaction(1),
        SessionTransaction::linear(session_id(), SessionRevision::new(2), entries),
    ] {
        store.commit(transaction).await.expect("commit");
    }
    drop(store);

    let log = std::fs::read_to_string(log_path(&root)).expect("read log");
    let last = log.lines().last().expect("cancel line");
    assert!(last.contains(r#""outcome":"cancelled","reason":"timeout""#));
    assert!(last.contains(r#""status":"cancelled""#));
}

// ---------------------------------------------------------------- v1 / missing reason

#[tokio::test(flavor = "multi_thread")]
async fn schema_v1_cancelled_file_is_unsupported() {
    let root = TempRoot::new();
    let dir = root.path.join("s-golden");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("log.jsonl"),
        concat!(
            r#"{"v":1,"revision":1,"entries":[{"id":"golden:entry:1","kind":{"type":"operation_started","operation_id":"op-1"}},{"id":"golden:entry:2","parent":"golden:entry:1","kind":{"type":"turn_started","operation_id":"op-1","turn_id":"turn-1"}},{"id":"golden:entry:3","parent":"golden:entry:2","kind":{"type":"user_message","turn_id":"turn-1","content":"hi"}}]}"#,
            "\n",
            r#"{"v":1,"revision":2,"entries":[{"id":"golden:entry:4","parent":"golden:entry:3","kind":{"type":"turn_terminated","turn_id":"turn-1","outcome":"cancelled"}},{"id":"golden:entry:5","parent":"golden:entry:4","kind":{"type":"operation_settled","operation_id":"op-1","outcome":"cancelled"}}]}"#,
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

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_outcome_without_reason_is_corrupt() {
    let root = TempRoot::new();
    let dir = root.path.join("s-golden");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("log.jsonl"),
        concat!(
            r#"{"v":2,"revision":1,"entries":[{"id":"golden:entry:1","kind":{"type":"operation_started","operation_id":"op-1"}},{"id":"golden:entry:2","parent":"golden:entry:1","kind":{"type":"turn_started","operation_id":"op-1","turn_id":"turn-1"}},{"id":"golden:entry:3","parent":"golden:entry:2","kind":{"type":"user_message","turn_id":"turn-1","parts":[{"type":"text","text":"hi"}]}},{"id":"golden:entry:4","parent":"golden:entry:3","kind":{"type":"turn_terminated","turn_id":"turn-1","outcome":"cancelled"}},{"id":"golden:entry:5","parent":"golden:entry:4","kind":{"type":"operation_settled","operation_id":"op-1","outcome":"cancelled"}}]}"#,
            "\n",
        ),
    )
    .expect("write v2 cancelled without reason");

    let store = JsonlSessionStore::open(&root.path).expect("open");
    let error = store.recover_session(&session_id()).expect_err("refused");
    let JsonlOpenError::Corrupt { line, reason } = error else {
        panic!("expected Corrupt, got {error:?}");
    };
    assert_eq!(line, 1);
    assert!(
        reason.contains("reason"),
        "names the missing reason: {reason}"
    );
}

// ---------------------------------------------------------------- 往返一致

#[tokio::test(flavor = "multi_thread")]
async fn seal_roundtrips_across_a_restart_with_identical_projection() {
    let root = TempRoot::new();
    {
        let store = JsonlSessionStore::open(&root.path).expect("open");
        for transaction in [
            start_transaction(0),
            batch_transaction(1),
            seal_transaction(2),
        ] {
            store.commit(transaction).await.expect("commit");
        }
    }

    let reopened = JsonlSessionStore::open(&root.path).expect("re-open");
    let disk_view = reopened.context_view(&session_id()).await.expect("view");

    let memory = MemorySessionStore::new();
    for transaction in [
        start_transaction(0),
        batch_transaction(1),
        seal_transaction(2),
    ] {
        memory.commit(transaction).await.expect("memory commit");
    }
    let memory_view = memory.context_view(&session_id()).await.expect("view");

    assert_eq!(disk_view, memory_view, "replay equals in-memory projection");
    assert!(disk_view.open_turns().is_empty());
}

// ---------------------------------------------------------------- 崩溃残留构造

#[tokio::test(flavor = "multi_thread")]
async fn crash_remnant_log_reports_open_turns_after_reopen() {
    let root = TempRoot::new();
    // B_k durable, no results, no terminal facts: exactly what a crash
    // between Barrier B_k and C_k leaves behind. Built by committing the
    // first two transactions and never sealing.
    {
        let store = JsonlSessionStore::open(&root.path).expect("open");
        store.commit(start_transaction(0)).await.expect("start");
        store.commit(batch_transaction(1)).await.expect("batch");
    }

    let reopened = JsonlSessionStore::open(&root.path).expect("re-open");
    let view = reopened.context_view(&session_id()).await.expect("view");
    let open = view.open_turns();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].turn_id(), &TurnId::new("turn-1"));
    assert_eq!(open[0].operation_id(), &OperationId::new("op-1"));
    let batch = open[0].unfilled_batch().expect("stranded batch");
    assert_eq!(batch.tool_batch_id(), &ToolBatchId::new("batch-1"));
    assert_eq!(
        batch.unfilled_call_ids(),
        &[ToolCallId::new("call-1"), ToolCallId::new("call-2")]
    );

    // The seal transaction the runtime would construct commits cleanly.
    reopened
        .commit(seal_transaction(2))
        .await
        .expect("seal commits");
    let sealed = reopened.context_view(&session_id()).await.expect("view");
    assert!(sealed.open_turns().is_empty());
}
