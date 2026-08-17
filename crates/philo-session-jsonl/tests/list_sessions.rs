//! JSONL-004: read-only session enumeration — canonical decoding, silent
//! skipping of foreign entries, lock-free coexistence, and zero disk writes.

use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll, Waker};

use philo_session::{
    OperationId, SessionAssistantBlock, SessionEntryKind, SessionId, SessionRevision, SessionStore,
    SessionTransaction, SessionUserPart, TurnId,
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
            "philo-session-jsonl-m9-{}-{}",
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

fn start_transaction(session_id: &SessionId, operation: &str, turn: &str) -> SessionTransaction {
    SessionTransaction::linear(
        session_id.clone(),
        SessionRevision::ZERO,
        vec![
            SessionEntryKind::OperationStarted {
                operation_id: OperationId::new(operation),
            },
            SessionEntryKind::TurnStarted {
                operation_id: OperationId::new(operation),
                turn_id: TurnId::new(turn),
            },
            SessionEntryKind::UserMessage {
                turn_id: TurnId::new(turn),
                parts: SessionUserPart::text_parts("hi"),
            },
        ],
    )
}

fn sorted_ids(store: &JsonlSessionStore) -> Vec<String> {
    let mut ids: Vec<String> = store
        .list_sessions()
        .expect("list sessions")
        .iter()
        .map(|id| id.as_str().to_owned())
        .collect();
    ids.sort();
    ids
}

#[test]
fn empty_root_lists_nothing() {
    let root = TempRoot::new();
    let store = JsonlSessionStore::open(&root.path).expect("open");
    assert!(store.list_sessions().expect("list").is_empty());
}

#[test]
fn multiple_sessions_are_enumerated_with_decoded_ids() {
    let root = TempRoot::new();
    let store = JsonlSessionStore::open(&root.path).expect("open");
    // Ids exercising the full encoding surface: plain, uppercase, unicode.
    for (index, id) in ["alpha", "Team/Chat:01 你", "b-2_x"].iter().enumerate() {
        let session = SessionId::new(*id);
        block_on(store.commit(start_transaction(
            &session,
            &format!("op-{index}"),
            &format!("turn-{index}"),
        )))
        .expect("commit");
    }

    assert_eq!(sorted_ids(&store), ["Team/Chat:01 你", "alpha", "b-2_x"]);
}

#[test]
fn foreign_entries_and_non_canonical_names_are_skipped() {
    let root = TempRoot::new();
    let store = JsonlSessionStore::open(&root.path).expect("open");
    let session = SessionId::new("real");
    block_on(store.commit(start_transaction(&session, "op-1", "turn-1"))).expect("commit");

    // Foreign directory, plain file, missing prefix, and a non-canonical
    // escape ("s-%61" decodes to "a" but re-encodes to "s-a"): all skipped.
    std::fs::create_dir(root.path.join("artifacts")).expect("foreign dir");
    std::fs::create_dir(root.path.join("s-%61")).expect("non-canonical dir");
    std::fs::create_dir(root.path.join("nonsense")).expect("unrelated dir");
    std::fs::write(root.path.join("s-fake"), b"a file, not a dir").expect("plain file");

    assert_eq!(sorted_ids(&store), ["real"]);
}

#[test]
fn enumeration_coexists_with_an_active_writer_and_writes_nothing() {
    let root = TempRoot::new();
    let writer = JsonlSessionStore::open(&root.path).expect("open writer");
    let session = SessionId::new("locked-session");
    block_on(writer.commit(start_transaction(&session, "op-1", "turn-1"))).expect("commit");

    // A second store instance enumerates while the writer holds the lock:
    // no lock conflict, because listing takes no session locks.
    let reader = JsonlSessionStore::open(&root.path).expect("open reader");
    let before = std::fs::read(root.path.join("s-locked-session").join("log.jsonl")).expect("log");
    assert_eq!(sorted_ids(&reader), ["locked-session"]);
    let after = std::fs::read(root.path.join("s-locked-session").join("log.jsonl")).expect("log");
    assert_eq!(before, after, "listing writes nothing");

    // The original writer still commits fine afterwards.
    block_on(writer.commit(SessionTransaction::linear(
        session,
        SessionRevision::new(1),
        vec![SessionEntryKind::AssistantMessage {
            turn_id: TurnId::new("turn-1"),
            blocks: vec![SessionAssistantBlock::Text {
                text: "hello".to_owned(),
            }],
        }],
    )))
    .expect("writer unaffected");
}

#[test]
fn listing_does_not_trigger_recovery() {
    let root = TempRoot::new();
    {
        let store = JsonlSessionStore::open(&root.path).expect("open");
        let session = SessionId::new("torn");
        block_on(store.commit(start_transaction(&session, "op-1", "turn-1"))).expect("commit");
    }
    // Simulate crash residue: a torn tail that recovery would truncate.
    let log = root.path.join("s-torn").join("log.jsonl");
    let mut bytes = std::fs::read(&log).expect("read log");
    bytes.extend_from_slice(br#"{"v":2,"revision":2,"entr"#);
    std::fs::write(&log, &bytes).expect("write torn tail");

    let store = JsonlSessionStore::open(&root.path).expect("re-open");
    assert_eq!(sorted_ids(&store), ["torn"]);
    assert_eq!(
        std::fs::read(&log).expect("log unchanged"),
        bytes,
        "listing never recovers or truncates"
    );
}

#[test]
fn image_bearing_sessions_enumerate_normally() {
    let root = TempRoot::new();
    let store = JsonlSessionStore::open(&root.path).expect("open");
    let session = SessionId::new("with-image");
    block_on(store.commit(SessionTransaction::linear(
        session.clone(),
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
                parts: vec![SessionUserPart::Image {
                    media_type: "image/png".to_owned(),
                    bytes: b"abc".to_vec(),
                }],
            },
        ],
    )))
    .expect("image commit");
    assert!(root.path.join("s-with-image").join("artifacts").is_dir());

    assert_eq!(sorted_ids(&store), ["with-image"]);
}
