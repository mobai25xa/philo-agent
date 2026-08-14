//! JSONL-006: Compaction entry schema v1 serialization, legacy compatibility,
//! and restart replay parity with the in-memory backend.

use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll, Waker};

use philo_session::{
    ContextMessage, EntryId, MemorySessionStore, OperationId, OperationOutcome, SessionEntryKind,
    SessionId, SessionRevision, SessionStore, SessionTransaction, SessionUserPart, TurnId,
    TurnOutcome,
};
use philo_session_jsonl::JsonlSessionStore;

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
            "philo-session-jsonl-m13-{}-{}",
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
    SessionId::new("m13-jsonl")
}

fn log_path(root: &TempRoot) -> PathBuf {
    root.path.join("s-m13-jsonl").join("log.jsonl")
}

fn successful_turn(revision: SessionRevision, number: usize) -> SessionTransaction {
    let operation_id = OperationId::new(format!("operation-{number}"));
    let turn_id = TurnId::new(format!("turn-{number}"));
    SessionTransaction::linear(
        session_id(),
        revision,
        vec![
            SessionEntryKind::OperationStarted {
                operation_id: operation_id.clone(),
            },
            SessionEntryKind::TurnStarted {
                operation_id: operation_id.clone(),
                turn_id: turn_id.clone(),
            },
            SessionEntryKind::UserMessage {
                turn_id: turn_id.clone(),
                parts: SessionUserPart::text_parts(format!("user-{number}")),
            },
            SessionEntryKind::AssistantMessage {
                turn_id: turn_id.clone(),
                content: format!("assistant-{number}"),
            },
            SessionEntryKind::TurnTerminated {
                turn_id,
                outcome: TurnOutcome::Succeeded,
            },
            SessionEntryKind::OperationSettled {
                operation_id,
                outcome: OperationOutcome::Succeeded,
            },
        ],
    )
}

fn compaction(
    revision: SessionRevision,
    summary: impl Into<String>,
    covers_up_to: EntryId,
) -> SessionTransaction {
    SessionTransaction::linear(
        session_id(),
        revision,
        vec![SessionEntryKind::Compaction {
            summary: summary.into(),
            covers_up_to,
        }],
    )
}

#[test]
fn golden_compaction_serializes_as_schema_v1_with_opaque_boundary() {
    let root = TempRoot::new();
    let store = JsonlSessionStore::open(&root.path).expect("open");
    let first =
        block_on(store.commit(successful_turn(SessionRevision::ZERO, 1))).expect("turn commits");
    let boundary = first
        .entries()
        .last()
        .expect("turn settles its operation")
        .id()
        .clone();
    block_on(store.commit(compaction(
        first.revision(),
        "summary \"quoted\"\nnext",
        boundary,
    )))
    .expect("compaction commits");
    drop(store);

    let log = std::fs::read_to_string(log_path(&root)).expect("read log");
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(
        lines[1],
        r#"{"v":1,"revision":2,"entries":[{"id":"m13-jsonl:entry:7","parent":"m13-jsonl:entry:6","kind":{"type":"compaction","summary":"summary \"quoted\"\nnext","covers_up_to":"m13-jsonl:entry:6"}}]}"#
    );
}

#[test]
fn pre_compaction_schema_v1_log_still_opens_and_replays() {
    let root = TempRoot::new();
    let dir = root.path.join("s-m13-jsonl");
    std::fs::create_dir_all(&dir).expect("create session dir");
    std::fs::write(
        dir.join("log.jsonl"),
        concat!(
            r#"{"v":1,"revision":1,"entries":[{"id":"m13-jsonl:entry:1","kind":{"type":"operation_started","operation_id":"operation-1"}},{"id":"m13-jsonl:entry:2","parent":"m13-jsonl:entry:1","kind":{"type":"turn_started","operation_id":"operation-1","turn_id":"turn-1"}},{"id":"m13-jsonl:entry:3","parent":"m13-jsonl:entry:2","kind":{"type":"user_message","turn_id":"turn-1","content":"legacy user"}},{"id":"m13-jsonl:entry:4","parent":"m13-jsonl:entry:3","kind":{"type":"assistant_message","turn_id":"turn-1","content":"legacy assistant"}},{"id":"m13-jsonl:entry:5","parent":"m13-jsonl:entry:4","kind":{"type":"turn_terminated","turn_id":"turn-1","outcome":"succeeded"}},{"id":"m13-jsonl:entry:6","parent":"m13-jsonl:entry:5","kind":{"type":"operation_settled","operation_id":"operation-1","outcome":"succeeded"}}]}"#,
            "\n",
        ),
    )
    .expect("write legacy log");

    let store = JsonlSessionStore::open(&root.path).expect("open");
    let view = block_on(store.context_view(&session_id())).expect("replay legacy log");
    assert_eq!(view.revision(), SessionRevision::new(1));
    assert_eq!(
        view.messages(),
        [
            ContextMessage::User {
                parts: SessionUserPart::text_parts("legacy user"),
            },
            ContextMessage::Assistant {
                content: "legacy assistant".to_owned(),
            },
        ]
    );
}

#[test]
fn compactions_roundtrip_across_restart_with_latest_boundary() {
    let root = TempRoot::new();
    let disk = JsonlSessionStore::open(&root.path).expect("open disk store");
    let memory = MemorySessionStore::new();

    let first_transaction = successful_turn(SessionRevision::ZERO, 1);
    let disk_first = block_on(disk.commit(first_transaction.clone())).expect("disk first turn");
    let memory_first = block_on(memory.commit(first_transaction)).expect("memory first turn");
    assert_eq!(disk_first, memory_first);
    let first_boundary = disk_first
        .entries()
        .last()
        .expect("first boundary")
        .id()
        .clone();

    let second_transaction = successful_turn(disk_first.revision(), 2);
    let disk_second = block_on(disk.commit(second_transaction.clone())).expect("disk second turn");
    let memory_second = block_on(memory.commit(second_transaction)).expect("memory second turn");
    assert_eq!(disk_second, memory_second);
    let second_boundary = disk_second
        .entries()
        .last()
        .expect("second boundary")
        .id()
        .clone();

    let first_compaction = compaction(disk_second.revision(), "summary-1", first_boundary);
    let disk_compaction =
        block_on(disk.commit(first_compaction.clone())).expect("disk first compaction");
    let memory_compaction =
        block_on(memory.commit(first_compaction)).expect("memory first compaction");
    assert_eq!(disk_compaction, memory_compaction);

    let third_transaction = successful_turn(disk_compaction.revision(), 3);
    let disk_third = block_on(disk.commit(third_transaction.clone())).expect("disk third turn");
    let memory_third = block_on(memory.commit(third_transaction)).expect("memory third turn");
    assert_eq!(disk_third, memory_third);

    let second_compaction = compaction(disk_third.revision(), "summary-2", second_boundary);
    let disk_latest =
        block_on(disk.commit(second_compaction.clone())).expect("disk second compaction");
    let memory_latest =
        block_on(memory.commit(second_compaction)).expect("memory second compaction");
    assert_eq!(disk_latest, memory_latest);
    drop(disk);

    let reopened = JsonlSessionStore::open(&root.path).expect("re-open disk store");
    let disk_view = block_on(reopened.context_view(&session_id())).expect("replayed view");
    let memory_view = block_on(memory.context_view(&session_id())).expect("memory view");
    assert_eq!(disk_view, memory_view);
    assert_eq!(
        disk_view.messages(),
        [
            ContextMessage::Summary {
                text: "summary-2".to_owned(),
            },
            ContextMessage::User {
                parts: SessionUserPart::text_parts("user-3"),
            },
            ContextMessage::Assistant {
                content: "assistant-3".to_owned(),
            },
        ],
        "only the latest compaction boundary controls the replayed projection"
    );
}
