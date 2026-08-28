//! JSONL-001: golden schema v2 format, recovery, locking, and backend parity.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use philo_session::{
    MemorySessionStore, OperationId, OperationOutcome, SessionAssistantBlock, SessionEntryKind,
    SessionError, SessionId, SessionRevision, SessionStore, SessionTokenUsage,
    SessionToolCall, SessionToolResult, SessionTransaction, SessionUserPart, ToolBatchId,
    ToolCallId, TurnId, TurnOutcome,
};
use philo_session_jsonl::{JsonlOpenError, JsonlSessionStore};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-session-jsonl-{}-{}",
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

fn session_id() -> SessionId {
    SessionId::new("golden")
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
                parts: SessionUserPart::text_parts("read a.txt"),
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
                SessionAssistantBlock::Text {
                    text: "I'll read that.".to_owned(),
                },
                SessionAssistantBlock::ToolCall(SessionToolCall::new(
                    ToolCallId::new("call-1"),
                    "read",
                    r#"{"path":"a.txt"}"#,
                )),
            ],
        }],
    )
}

fn results_transaction(revision: u64) -> SessionTransaction {
    SessionTransaction::linear(
        session_id(),
        SessionRevision::new(revision),
        vec![SessionEntryKind::ToolResult {
            turn_id: TurnId::new("turn-1"),
            tool_batch_id: ToolBatchId::new("batch-1"),
            result: SessionToolResult::error(
                ToolCallId::new("call-1"),
                "not_found",
                "file not found",
            ),
        }],
    )
}

fn settle_transaction(revision: u64) -> SessionTransaction {
    SessionTransaction::linear(
        session_id(),
        SessionRevision::new(revision),
        vec![
            SessionEntryKind::AssistantMessage {
                turn_id: TurnId::new("turn-1"),
                blocks: vec![SessionAssistantBlock::Text {
                    text: "done".to_owned(),
                }],
            },
            SessionEntryKind::TurnTerminated {
                turn_id: TurnId::new("turn-1"),
                outcome: TurnOutcome::Succeeded,
            },
            SessionEntryKind::OperationSettled {
                operation_id: OperationId::new("op-1"),
                outcome: OperationOutcome::Succeeded,
                usage: None,
            },
        ],
    )
}

async fn commit_full_turn(store: &dyn SessionStore) {
    for transaction in [
        start_transaction(0),
        batch_transaction(1),
        results_transaction(2),
        settle_transaction(3),
    ] {
        store
            .commit(transaction)
            .await
            .expect("valid transaction commits");
    }
}

fn log_path(root: &TempRoot) -> PathBuf {
    root.path.join("s-golden").join("log.jsonl")
}

// --- Golden format (M5-007 schema pinning) ----------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn golden_schema_v2_format_and_directory_encoding() {
    let root = TempRoot::new();
    let store = JsonlSessionStore::open(&root.path).expect("open");
    commit_full_turn(&store).await;
    drop(store);

    // Directory encoding is pinned: lowercase/digits/-/_ pass, rest is %XX.
    assert!(root.path.join("s-golden").is_dir());
    assert!(root.path.join("s-golden").join("lock").is_file());

    let log = std::fs::read_to_string(log_path(&root)).expect("read log");
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 4);
    assert_eq!(
        lines[0],
        r#"{"v":2,"revision":1,"entries":[{"id":"golden:entry:1","kind":{"type":"operation_started","operation_id":"op-1"}},{"id":"golden:entry:2","parent":"golden:entry:1","kind":{"type":"turn_started","operation_id":"op-1","turn_id":"turn-1"}},{"id":"golden:entry:3","parent":"golden:entry:2","kind":{"type":"user_message","turn_id":"turn-1","parts":[{"type":"text","text":"read a.txt"}]}}]}"#
    );
    assert_eq!(
        lines[1],
        r#"{"v":2,"revision":2,"entries":[{"id":"golden:entry:4","parent":"golden:entry:3","kind":{"type":"assistant_tool_call_batch","turn_id":"turn-1","model_call_id":"model-call-1","tool_batch_id":"batch-1","blocks":[{"type":"text","text":"I'll read that."},{"type":"tool_call","id":"call-1","name":"read","arguments":"{\"path\":\"a.txt\"}"}]}}]}"#
    );
    assert_eq!(
        lines[2],
        r#"{"v":2,"revision":3,"entries":[{"id":"golden:entry:5","parent":"golden:entry:4","kind":{"type":"tool_result","turn_id":"turn-1","tool_batch_id":"batch-1","result":{"call_id":"call-1","status":"error","code":"not_found","message":"file not found"}}}]}"#
    );
    assert_eq!(
        lines[3],
        r#"{"v":2,"revision":4,"entries":[{"id":"golden:entry:6","parent":"golden:entry:5","kind":{"type":"assistant_message","turn_id":"turn-1","blocks":[{"type":"text","text":"done"}]}},{"id":"golden:entry:7","parent":"golden:entry:6","kind":{"type":"turn_terminated","turn_id":"turn-1","outcome":"succeeded"}},{"id":"golden:entry:8","parent":"golden:entry:7","kind":{"type":"operation_settled","operation_id":"op-1","outcome":"succeeded"}}]}"#
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn golden_directory_encoding_escapes_unsafe_and_uppercase_bytes() {
    let root = TempRoot::new();
    let store = JsonlSessionStore::open(&root.path).expect("open");
    let id = SessionId::new("Team/Chat:01 你");
    store
        .commit(SessionTransaction::linear(
            id.clone(),
            SessionRevision::ZERO,
            vec![SessionEntryKind::OperationStarted {
                operation_id: OperationId::new("op-1"),
            }],
        ))
        .await
        .expect("commit");
    // "T"=%54, "/"=%2F, "C"=%43, ":"=%3A, " "=%20, "你"=%E4%BD%A0 (UTF-8).
    assert!(
        root.path
            .join("s-%54eam%2F%43hat%3A01%20%E4%BD%A0")
            .is_dir(),
        "deterministic reversible encoding"
    );
}

// --- Durability and restart continuation (M5-002 / M5-007) ------------------

#[tokio::test(flavor = "multi_thread")]
async fn golden_operation_settled_with_usage_serializes_and_round_trips() {
    let root = TempRoot::new();
    let store = JsonlSessionStore::open(&root.path).expect("open");
    let usage = SessionTokenUsage {
        input_tokens: Some(100),
        output_tokens: Some(50),
        cache_read_tokens: Some(200),
        cache_write_tokens: None,
        reasoning_tokens: Some(10),
    };
    let tx = SessionTransaction::linear(
        session_id(),
        SessionRevision::ZERO,
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
                parts: SessionUserPart::text_parts("hi"),
            },
            SessionEntryKind::AssistantMessage {
                turn_id: TurnId::new("turn-1"),
                blocks: vec![SessionAssistantBlock::Text {
                    text: "hello".into(),
                }],
            },
            SessionEntryKind::TurnTerminated {
                turn_id: TurnId::new("turn-1"),
                outcome: TurnOutcome::Succeeded,
            },
            SessionEntryKind::OperationSettled {
                operation_id: OperationId::new("op-1"),
                outcome: OperationOutcome::Succeeded,
                usage: Some(usage),
            },
        ],
    );
    let commit = store.commit(tx).await.expect("commit");
    assert_eq!(commit.revision().get(), 1);
    drop(store);

    // Pin the golden JSON line: usage fields are camelCase with
    // skip_serializing_if for None values.
    let log = std::fs::read_to_string(log_path(&root)).expect("read log");
    let lines: Vec<&str> = log.lines().collect();
    let line = lines.last().expect("transaction line");
    assert!(line.contains("\"operation_settled\""));
    assert!(line.contains("\"usage\":{"));
    assert!(line.contains("\"input_tokens\":100"));
    assert!(line.contains("\"output_tokens\":50"));
    assert!(line.contains("\"cache_read_tokens\":200"));
    assert!(line.contains("\"reasoning_tokens\":10"));
    assert!(!line.contains("\"cache_write_tokens\""));

    // Round-trip: reopen and verify the usage survives.
    let store = JsonlSessionStore::open(&root.path).expect("reopen");
    let view = store.context_view(&session_id()).await.expect("view");
    assert_eq!(view.latest_usage(), Some(usage));
}

#[tokio::test(flavor = "multi_thread")]
async fn committed_transactions_are_visible_to_a_fresh_store_instance() {
    let root = TempRoot::new();
    {
        let store = JsonlSessionStore::open(&root.path).expect("open");
        commit_full_turn(&store).await;
    }
    let reopened = JsonlSessionStore::open(&root.path).expect("re-open");
    let view = reopened.context_view(&session_id()).await.expect("view");
    assert_eq!(view.revision(), SessionRevision::new(4));
    assert_eq!(view.messages().len(), 4, "user, calls, result, assistant");
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_continues_revisions_and_entry_ids_seamlessly() {
    let root = TempRoot::new();
    {
        let store = JsonlSessionStore::open(&root.path).expect("open");
        for transaction in [start_transaction(0), batch_transaction(1)] {
            store.commit(transaction).await.expect("commit");
        }
    }
    let reopened = JsonlSessionStore::open(&root.path).expect("re-open");
    let commit = reopened
        .commit(results_transaction(2))
        .await
        .expect("commit continues");
    assert_eq!(commit.revision(), SessionRevision::new(3));
    assert_eq!(commit.entries()[0].id().as_str(), "golden:entry:5");
    reopened
        .commit(settle_transaction(3))
        .await
        .expect("settle");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_session_context_view_creates_no_directory() {
    let root = TempRoot::new();
    let store = JsonlSessionStore::open(&root.path).expect("open");
    let view = store
        .context_view(&SessionId::new("ghost"))
        .await
        .expect("view");
    assert_eq!(view.revision(), SessionRevision::ZERO);
    assert!(view.messages().is_empty());
    assert!(!root.path.join("s-ghost").exists());
}

// --- Torn tail (M5-003) ------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn torn_tail_is_truncated_reported_and_appendable() {
    let root = TempRoot::new();
    {
        let store = JsonlSessionStore::open(&root.path).expect("open");
        for transaction in [start_transaction(0), batch_transaction(1)] {
            store.commit(transaction).await.expect("commit");
        }
    }
    // Simulate a crash mid-append: a partial line without a terminator.
    let intact_len = std::fs::metadata(log_path(&root)).expect("meta").len();
    let mut bytes = std::fs::read(log_path(&root)).expect("read");
    bytes.extend_from_slice(br#"{"v":2,"revision":3,"entr"#);
    std::fs::write(log_path(&root), &bytes).expect("write torn tail");

    let reopened = JsonlSessionStore::open(&root.path).expect("re-open");
    let report = reopened.recover_session(&session_id()).expect("recover");
    assert_eq!(
        report.transactions(),
        2,
        "revision back to the last complete transaction"
    );
    assert!(report.tail_was_truncated());
    assert_eq!(report.truncated_tail_bytes(), 25);
    assert_eq!(
        std::fs::metadata(log_path(&root)).expect("meta").len(),
        intact_len,
        "torn bytes are physically removed"
    );

    // Later commits append normally from the recovered revision.
    let commit = reopened
        .commit(results_transaction(2))
        .await
        .expect("commit after recovery");
    assert_eq!(commit.revision(), SessionRevision::new(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn torn_json_with_terminator_at_eof_is_crash_residue() {
    let root = TempRoot::new();
    {
        let store = JsonlSessionStore::open(&root.path).expect("open");
        store.commit(start_transaction(0)).await.expect("commit");
    }
    let mut bytes = std::fs::read(log_path(&root)).expect("read");
    bytes.extend_from_slice(b"{\"v\":2,\"revision\":2\n");
    std::fs::write(log_path(&root), &bytes).expect("write");

    let reopened = JsonlSessionStore::open(&root.path).expect("re-open");
    let report = reopened.recover_session(&session_id()).expect("recover");
    assert_eq!(report.transactions(), 1);
    assert!(report.tail_was_truncated());
}

// --- Mid-log corruption (M5-004) ---------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn mid_log_corruption_refuses_to_open_with_the_line_number() {
    let root = TempRoot::new();
    {
        let store = JsonlSessionStore::open(&root.path).expect("open");
        for transaction in [start_transaction(0), batch_transaction(1)] {
            store.commit(transaction).await.expect("commit");
        }
    }
    // Corrupt line 1 while line 2 stays complete: never silently repaired.
    let log = std::fs::read_to_string(log_path(&root)).expect("read");
    let lines: Vec<&str> = log.lines().collect();
    let forged = format!("this is not json\n{}\n", lines[1]);
    std::fs::write(log_path(&root), forged).expect("write");

    let reopened = JsonlSessionStore::open(&root.path).expect("open store");
    let error = reopened
        .recover_session(&session_id())
        .expect_err("refused");
    let JsonlOpenError::Corrupt { line, .. } = error else {
        panic!("expected Corrupt, got {error:?}");
    };
    assert_eq!(line, 1);

    // The trait path reports the same condition as a store failure.
    let trait_error = reopened
        .context_view(&session_id())
        .await
        .expect_err("refused");
    assert!(matches!(trait_error, SessionError::StoreUnavailable { .. }));
}

#[tokio::test(flavor = "multi_thread")]
async fn revision_discontinuity_is_corruption() {
    let root = TempRoot::new();
    {
        let store = JsonlSessionStore::open(&root.path).expect("open");
        for transaction in [start_transaction(0), batch_transaction(1)] {
            store.commit(transaction).await.expect("commit");
        }
    }
    // Drop line 1, keeping line 2 whose revision no longer matches its position.
    let log = std::fs::read_to_string(log_path(&root)).expect("read");
    let second = log.lines().nth(1).expect("two lines");
    std::fs::write(log_path(&root), format!("{second}\n")).expect("write");

    let reopened = JsonlSessionStore::open(&root.path).expect("open store");
    let error = reopened
        .recover_session(&session_id())
        .expect_err("refused");
    assert!(matches!(error, JsonlOpenError::Corrupt { line: 1, .. }));
}

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_schema_version_refuses_to_open() {
    let root = TempRoot::new();
    let dir = root.path.join("s-golden");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("log.jsonl"),
        concat!(
            r#"{"v":1,"revision":1,"entries":[{"id":"golden:entry:1","kind":{"type":"operation_started","operation_id":"op-1"}}]}"#,
            "\n",
        ),
    )
    .expect("write v1 file");

    let store = JsonlSessionStore::open(&root.path).expect("open store");
    let error = store.recover_session(&session_id()).expect_err("refused");
    assert!(matches!(
        error,
        JsonlOpenError::UnsupportedSchema { found: 1 }
    ));
}

// --- Single-writer lock (M5-005) ---------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn second_writer_is_rejected_and_release_allows_reopen() {
    let root = TempRoot::new();
    let first = JsonlSessionStore::open(&root.path).expect("open first");
    first.commit(start_transaction(0)).await.expect("commit");

    let second = JsonlSessionStore::open(&root.path).expect("open second store");
    let error = second.recover_session(&session_id()).expect_err("locked");
    assert!(
        matches!(error, JsonlOpenError::Locked { .. }),
        "lock conflicts are distinguishable: {error:?}"
    );
    let trait_error = second
        .commit(batch_transaction(1))
        .await
        .expect_err("locked");
    let SessionError::StoreUnavailable { reason } = &trait_error else {
        panic!("expected StoreUnavailable, got {trait_error:?}");
    };
    assert!(
        reason.contains("locked"),
        "diagnostic text names the lock: {reason}"
    );

    // Releasing the first writer frees the session for the second.
    drop(first);
    let report = second
        .recover_session(&session_id())
        .expect("reopen after release");
    assert_eq!(report.transactions(), 1);
    second
        .commit(batch_transaction(1))
        .await
        .expect("commit after takeover");
}

// --- Backend parity (M5-006) --------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn jsonl_and_memory_backends_agree_on_facts_and_errors() {
    let root = TempRoot::new();
    let jsonl = JsonlSessionStore::open(&root.path).expect("open");
    let memory = MemorySessionStore::new();

    for transaction in [
        start_transaction(0),
        batch_transaction(1),
        results_transaction(2),
        settle_transaction(3),
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

    // The same invalid transaction is rejected with the same validation error.
    let invalid = SessionTransaction::linear(
        session_id(),
        SessionRevision::new(4),
        vec![SessionEntryKind::UserMessage {
            turn_id: TurnId::new("turn-1"),
            parts: SessionUserPart::text_parts("turn already terminated"),
        }],
    );
    let jsonl_error = jsonl.commit(invalid.clone()).await.expect_err("rejected");
    let memory_error = memory.commit(invalid).await.expect_err("rejected");
    assert_eq!(jsonl_error, memory_error);

    // Same revision-conflict classification too.
    let stale = jsonl.commit(start_transaction(0)).await.expect_err("stale");
    assert!(matches!(stale, SessionError::RevisionConflict { .. }));
}

// --- Misc --------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn empty_log_after_crash_between_dir_and_first_commit_is_an_empty_session() {
    let root = TempRoot::new();
    // Simulate: directory and lock exist, log was never created.
    std::fs::create_dir_all(root.path.join("s-golden")).expect("mkdir");
    let store = JsonlSessionStore::open(&root.path).expect("open");
    let report = store.recover_session(&session_id()).expect("recover");
    assert_eq!(report.transactions(), 0);
    assert!(!report.tail_was_truncated());
    store
        .commit(start_transaction(0))
        .await
        .expect("first commit works");
}

#[tokio::test(flavor = "multi_thread")]
async fn recover_session_without_directory_reports_zero_and_creates_nothing() {
    let root = TempRoot::new();
    let store = JsonlSessionStore::open(&root.path).expect("open");
    let report = store
        .recover_session(&SessionId::new("ghost"))
        .expect("recover");
    assert_eq!(report.transactions(), 0);
    assert!(!root.path.join("s-ghost").exists());
}
