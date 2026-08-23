//! Session titles through the durable backend: derived names land in the
//! listing sidecar after the first commit, renames append `TitleSet`
//! transactions, and a reopened store reports the same titles.

use std::future::Future;
use std::path::Path;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use philo_session::{
    OperationId, SessionEntryKind, SessionId, SessionRevision, SessionStore, SessionSummary,
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

fn commit(
    store: &JsonlSessionStore,
    session: &str,
    revision: u64,
    kinds: Vec<SessionEntryKind>,
) {
    block_on(store.commit(SessionTransaction::linear(
        SessionId::new(session),
        SessionRevision::new(revision),
        kinds,
    )))
    .expect("commit");
}

fn first_user_message(text: &str) -> Vec<SessionEntryKind> {
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
            parts: SessionUserPart::text_parts(text),
        },
    ]
}

fn sorted_summaries(root: &Path) -> Vec<SessionSummary> {
    let store = JsonlSessionStore::open(root).expect("open store");
    let mut summaries = store.list_session_summaries().expect("summaries");
    summaries.sort_by(|left, right| left.session_id.as_str().cmp(right.session_id.as_str()));
    let _ = store.shutdown();
    summaries
}

#[test]
fn titles_survive_rename_and_reopen() {
    let root = tempfile_dir("jsonl-title-rename");
    let store = JsonlSessionStore::open(&root).expect("open store");

    commit(&store, "sess-a", 0, first_user_message("fix   the\nlogin bug"));
    commit(
        &store,
        "sess-b",
        0,
        vec![SessionEntryKind::OperationStarted {
            operation_id: OperationId::new("op-b"),
        }],
    );
    assert_eq!(
        sorted_summaries(&root),
        [
            SessionSummary {
                session_id: SessionId::new("sess-a"),
                title: Some("fix the login bug".into()),
            },
            SessionSummary {
                session_id: SessionId::new("sess-b"),
                title: None,
            },
        ],
        "derived title lands in the sidecar at commit time"
    );

    // Rename appends a durable TitleSet transaction.
    commit(
        &store,
        "sess-a",
        1,
        vec![SessionEntryKind::TitleSet {
            title: "auth deep dive".into(),
        }],
    );
    let _ = store.shutdown();

    // A fresh instance recovers from the log and rewrites the same sidecar.
    assert_eq!(
        sorted_summaries(&root),
        [
            SessionSummary {
                session_id: SessionId::new("sess-a"),
                title: Some("auth deep dive".into()),
            },
            SessionSummary {
                session_id: SessionId::new("sess-b"),
                title: None,
            },
        ],
        "the override wins after recovery"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_missing_sidecar_only_costs_the_title() {
    let root = tempfile_dir("jsonl-title-sidecar");
    let store = JsonlSessionStore::open(&root).expect("open store");
    commit(&store, "sess-c", 0, first_user_message("hello world"));
    drop(store);

    // Simulate a crash between log append and sidecar rename: listings stay
    // correct by falling back to ids, and any later touch heals the cache.
    let session_dir = std::fs::read_dir(&root)
        .expect("root")
        .find_map(|entry| entry.expect("entry").file_name().to_str().map(str::to_owned))
        .expect("session dir");
    let sidecar = root.join(&session_dir).join("title");
    assert!(sidecar.exists(), "sidecar written at commit time");
    std::fs::remove_file(&sidecar).expect("remove sidecar");

    let summaries = sorted_summaries(&root);
    assert_eq!(
        summaries,
        [SessionSummary {
            session_id: SessionId::new("sess-c"),
            title: None,
        }]
    );
    let _ = std::fs::remove_dir_all(&root);
}

fn tempfile_dir(label: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
        "philo-session-jsonl-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create temp dir");
    path
}
