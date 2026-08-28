//! JSONL-003: content-addressed artifact storage, UserMessage `parts` on
//! schema v2, and recovery semantics (ADR-0002).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use philo_session::{
    ContextMessage, MemorySessionStore, OperationId, OperationOutcome, SessionAssistantBlock,
    SessionEntryKind, SessionId, SessionRevision, SessionStore, SessionTransaction,
    SessionUserPart, TurnId, TurnOutcome,
};
use philo_session_jsonl::{JsonlOpenError, JsonlSessionStore};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-session-jsonl-m8-{}-{}",
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

fn session_dir(root: &TempRoot) -> PathBuf {
    root.path.join("s-golden")
}

fn log_path(root: &TempRoot) -> PathBuf {
    session_dir(root).join("log.jsonl")
}

fn artifacts_dir(root: &TempRoot) -> PathBuf {
    session_dir(root).join("artifacts")
}

/// Known SHA-256 test vector: the "image" bytes are `abc`.
const IMAGE_BYTES: &[u8] = b"abc";
const IMAGE_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

fn image_parts() -> Vec<SessionUserPart> {
    vec![
        SessionUserPart::Text("look at this".to_owned()),
        SessionUserPart::Image {
            media_type: "image/png".to_owned(),
            bytes: IMAGE_BYTES.to_vec(),
        },
    ]
}

fn start_transaction(
    revision: u64,
    operation: &str,
    turn: &str,
    parts: Vec<SessionUserPart>,
) -> SessionTransaction {
    SessionTransaction::linear(
        session_id(),
        SessionRevision::new(revision),
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
                parts,
            },
        ],
    )
}

fn settle_transaction(revision: u64, operation: &str, turn: &str) -> SessionTransaction {
    SessionTransaction::linear(
        session_id(),
        SessionRevision::new(revision),
        vec![
            SessionEntryKind::AssistantMessage {
                turn_id: TurnId::new(turn),
                blocks: vec![SessionAssistantBlock::Text {
                    text: "a cat".to_owned(),
                }],
            },
            SessionEntryKind::TurnTerminated {
                turn_id: TurnId::new(turn),
                outcome: TurnOutcome::Succeeded,
            },
            SessionEntryKind::OperationSettled {
                operation_id: OperationId::new(operation),
                outcome: OperationOutcome::Succeeded,
                usage: None,
            },
        ],
    )
}

// --- Golden format and barrier (M8-002) ---------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn golden_image_transaction_stores_reference_and_fsynced_artifact() {
    let root = TempRoot::new();
    let store = JsonlSessionStore::open(&root.path).expect("open");
    store
        .commit(start_transaction(0, "op-1", "turn-1", image_parts()))
        .await
        .expect("image commit");
    drop(store);

    // The artifact file is durable and byte-identical, named by its hash.
    let artifact = artifacts_dir(&root).join(IMAGE_SHA256);
    assert_eq!(
        std::fs::read(&artifact).expect("artifact exists"),
        IMAGE_BYTES
    );

    // The log line carries only the reference; envelope is schema v2.
    let log = std::fs::read_to_string(log_path(&root)).expect("read log");
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines[0],
        format!(
            concat!(
                r#"{{"v":2,"revision":1,"entries":["#,
                r#"{{"id":"golden:entry:1","kind":{{"type":"operation_started","operation_id":"op-1"}}}},"#,
                r#"{{"id":"golden:entry:2","parent":"golden:entry:1","kind":{{"type":"turn_started","operation_id":"op-1","turn_id":"turn-1"}}}},"#,
                r#"{{"id":"golden:entry:3","parent":"golden:entry:2","kind":{{"type":"user_message","turn_id":"turn-1","parts":["#,
                r#"{{"type":"text","text":"look at this"}},"#,
                r#"{{"type":"image","media_type":"image/png","artifact":"{hash}","len":3}}"#,
                r#"]}}}}]}}"#
            ),
            hash = IMAGE_SHA256
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn plain_text_sessions_never_create_an_artifacts_directory() {
    let root = TempRoot::new();
    let store = JsonlSessionStore::open(&root.path).expect("open");
    store
        .commit(start_transaction(
            0,
            "op-1",
            "turn-1",
            SessionUserPart::text_parts("plain"),
        ))
        .await
        .expect("text commit");
    assert!(session_dir(&root).is_dir());
    assert!(!artifacts_dir(&root).exists());
}

// --- Restart replay fidelity (M8-003) ------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn image_parts_rebuild_byte_for_byte_across_a_restart() {
    let root = TempRoot::new();
    {
        let store = JsonlSessionStore::open(&root.path).expect("open");
        store
            .commit(start_transaction(0, "op-1", "turn-1", image_parts()))
            .await
            .expect("commit");
        store
            .commit(settle_transaction(1, "op-1", "turn-1"))
            .await
            .expect("settle");
    }
    let reopened = JsonlSessionStore::open(&root.path).expect("re-open");
    let view = reopened.context_view(&session_id()).await.expect("view");
    assert_eq!(view.revision(), SessionRevision::new(2));
    assert_eq!(
        view.messages()[0],
        ContextMessage::User {
            parts: image_parts()
        }
    );
}

// --- Content addressing deduplicates (ADR-0002 invariant 2) --------------------

#[tokio::test(flavor = "multi_thread")]
async fn resubmitting_the_same_image_stores_one_artifact() {
    let root = TempRoot::new();
    let store = JsonlSessionStore::open(&root.path).expect("open");
    store
        .commit(start_transaction(0, "op-1", "turn-1", image_parts()))
        .await
        .expect("first image turn");
    store
        .commit(settle_transaction(1, "op-1", "turn-1"))
        .await
        .expect("settle");
    store
        .commit(start_transaction(2, "op-2", "turn-2", image_parts()))
        .await
        .expect("second image turn");

    let files: Vec<_> = std::fs::read_dir(artifacts_dir(&root))
        .expect("artifacts dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(files, vec![IMAGE_SHA256.to_owned()]);
}

// --- Crash semantics (M8-004) ---------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn orphan_artifacts_are_tolerated_reported_and_kept() {
    let root = TempRoot::new();
    {
        let store = JsonlSessionStore::open(&root.path).expect("open");
        store
            .commit(start_transaction(0, "op-1", "turn-1", image_parts()))
            .await
            .expect("commit");
    }
    // Crash residue: an unreferenced artifact and an interrupted temp file.
    std::fs::write(artifacts_dir(&root).join("0000dead"), b"leftover").expect("orphan");
    std::fs::write(artifacts_dir(&root).join("ffff.tmp"), b"partial").expect("tmp residue");

    let reopened = JsonlSessionStore::open(&root.path).expect("re-open");
    let report = reopened.recover_session(&session_id()).expect("opens fine");
    assert_eq!(report.transactions(), 1);
    assert_eq!(
        report.orphan_artifacts(),
        &["0000dead".to_owned(), "ffff.tmp".to_owned()],
        "orphans are reported sorted; the referenced artifact is not listed"
    );
    // Tolerated, never deleted.
    assert!(artifacts_dir(&root).join("0000dead").is_file());
    assert!(artifacts_dir(&root).join("ffff.tmp").is_file());
    // The session keeps working.
    reopened
        .commit(settle_transaction(1, "op-1", "turn-1"))
        .await
        .expect("commit works");
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_referenced_artifact_refuses_to_open() {
    let root = TempRoot::new();
    {
        let store = JsonlSessionStore::open(&root.path).expect("open");
        store
            .commit(start_transaction(0, "op-1", "turn-1", image_parts()))
            .await
            .expect("commit");
    }
    std::fs::remove_file(artifacts_dir(&root).join(IMAGE_SHA256)).expect("delete artifact");

    let reopened = JsonlSessionStore::open(&root.path).expect("open store");
    let error = reopened
        .recover_session(&session_id())
        .expect_err("refused");
    let JsonlOpenError::Corrupt { line, reason } = error else {
        panic!("expected Corrupt, got {error:?}");
    };
    assert_eq!(line, 1);
    assert!(reason.contains("unreadable"), "names the failure: {reason}");
}

#[tokio::test(flavor = "multi_thread")]
async fn tampered_artifact_content_refuses_to_open() {
    let root = TempRoot::new();
    {
        let store = JsonlSessionStore::open(&root.path).expect("open");
        store
            .commit(start_transaction(0, "op-1", "turn-1", image_parts()))
            .await
            .expect("commit");
    }
    // Same length, different bytes: the recorded hash no longer matches.
    std::fs::write(artifacts_dir(&root).join(IMAGE_SHA256), b"abd").expect("tamper");

    let reopened = JsonlSessionStore::open(&root.path).expect("open store");
    let error = reopened
        .recover_session(&session_id())
        .expect_err("refused");
    let JsonlOpenError::Corrupt { reason, .. } = error else {
        panic!("expected Corrupt, got {error:?}");
    };
    assert!(reason.contains("hash"), "names the mismatch: {reason}");
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_length_artifact_refuses_to_open() {
    let root = TempRoot::new();
    {
        let store = JsonlSessionStore::open(&root.path).expect("open");
        store
            .commit(start_transaction(0, "op-1", "turn-1", image_parts()))
            .await
            .expect("commit");
    }
    std::fs::write(artifacts_dir(&root).join(IMAGE_SHA256), b"abcabc").expect("grow");

    let reopened = JsonlSessionStore::open(&root.path).expect("open store");
    let error = reopened
        .recover_session(&session_id())
        .expect_err("refused");
    let JsonlOpenError::Corrupt { reason, .. } = error else {
        panic!("expected Corrupt, got {error:?}");
    };
    assert!(reason.contains("length"), "names the mismatch: {reason}");
}

// --- v2 user_message shape (no legacy content) ---------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn schema_v1_legacy_content_file_is_unsupported() {
    let root = TempRoot::new();
    let dir = session_dir(&root);
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("log.jsonl"),
        concat!(
            r#"{"v":1,"revision":1,"entries":[{"id":"golden:entry:1","kind":{"type":"operation_started","operation_id":"op-1"}},{"id":"golden:entry:2","parent":"golden:entry:1","kind":{"type":"turn_started","operation_id":"op-1","turn_id":"turn-1"}},{"id":"golden:entry:3","parent":"golden:entry:2","kind":{"type":"user_message","turn_id":"turn-1","content":"hi"}}]}"#,
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
async fn user_message_with_both_content_and_parts_is_corrupt() {
    let root = TempRoot::new();
    let dir = session_dir(&root);
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("log.jsonl"),
        concat!(
            r#"{"v":2,"revision":1,"entries":[{"id":"golden:entry:1","kind":{"type":"operation_started","operation_id":"op-1"}},{"id":"golden:entry:2","parent":"golden:entry:1","kind":{"type":"turn_started","operation_id":"op-1","turn_id":"turn-1"}},{"id":"golden:entry:3","parent":"golden:entry:2","kind":{"type":"user_message","turn_id":"turn-1","content":"hi","parts":[{"type":"text","text":"hi"}]}}]}"#,
            "\n",
        ),
    )
    .expect("write ambiguous file");

    let store = JsonlSessionStore::open(&root.path).expect("open");
    let error = store.recover_session(&session_id()).expect_err("refused");
    assert!(matches!(error, JsonlOpenError::Corrupt { line: 1, .. }));
}

#[tokio::test(flavor = "multi_thread")]
async fn user_message_without_parts_is_corrupt() {
    let root = TempRoot::new();
    let dir = session_dir(&root);
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("log.jsonl"),
        concat!(
            r#"{"v":2,"revision":1,"entries":[{"id":"golden:entry:1","kind":{"type":"operation_started","operation_id":"op-1"}},{"id":"golden:entry:2","parent":"golden:entry:1","kind":{"type":"turn_started","operation_id":"op-1","turn_id":"turn-1"}},{"id":"golden:entry:3","parent":"golden:entry:2","kind":{"type":"user_message","turn_id":"turn-1"}}]}"#,
            "\n",
        ),
    )
    .expect("write file missing parts");

    let store = JsonlSessionStore::open(&root.path).expect("open");
    let error = store.recover_session(&session_id()).expect_err("refused");
    assert!(matches!(error, JsonlOpenError::Corrupt { line: 1, .. }));
}

// --- Backend parity extended to image parts -------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn jsonl_and_memory_backends_agree_on_image_facts() {
    let root = TempRoot::new();
    let jsonl = JsonlSessionStore::open(&root.path).expect("open");
    let memory = MemorySessionStore::new();

    for transaction in [
        start_transaction(0, "op-1", "turn-1", image_parts()),
        settle_transaction(1, "op-1", "turn-1"),
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
            .expect("memory view"),
        "identical context including image bytes"
    );
}
