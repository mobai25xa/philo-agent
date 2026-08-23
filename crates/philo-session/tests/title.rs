//! Session titles: derived display names, explicit `TitleSet` overrides,
//! validation, and summary listing through the memory backend.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use philo_session::{
    MemorySessionStore, OperationId, SessionEntryKind, SessionError, SessionId, SessionRevision,
    SessionStore, SessionSummary, SessionTransaction, SessionUserPart, TurnId,
    SessionValidationError,
};

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
    store: &MemorySessionStore,
    session: &str,
    revision: u64,
    kinds: Vec<SessionEntryKind>,
) -> Result<(), SessionError> {
    block_on(store.commit(SessionTransaction::linear(
        SessionId::new(session),
        SessionRevision::new(revision),
        kinds,
    )))
    .map(|_| ())
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

#[test]
fn the_first_user_text_derives_the_display_title() {
    let store = MemorySessionStore::new();
    commit(&store, "s", 0, first_user_message("fix   the\nlogin bug")).expect("commit");
    assert_eq!(
        block_on(store.list_session_summaries()).expect("summaries")[0].title,
        Some("fix the login bug".to_owned())
    );
}

#[test]
fn derived_titles_collapse_whitespace_and_truncate() {
    let store = MemorySessionStore::new();
    let long = "word ".repeat(30);
    commit(&store, "s", 0, first_user_message(long.trim())).expect("commit");
    let summaries = block_on(store.list_session_summaries()).expect("summaries");
    let title = summaries[0].title.as_deref().expect("derived title");
    assert!(title.ends_with('…'), "{title}");
    assert!(title.chars().count() <= 49, "{title}");
    assert!(!title.contains('\n'));
}

#[test]
fn an_image_only_first_message_has_no_title() {
    let store = MemorySessionStore::new();
    commit(
        &store,
        "s",
        0,
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
                    media_type: "image/png".into(),
                    bytes: vec![1, 2, 3],
                }],
            },
        ],
    )
    .expect("commit");
    let summaries = block_on(store.list_session_summaries()).expect("summaries");
    assert_eq!(summaries, [SessionSummary::untitled(SessionId::new("s"))]);
}

#[test]
fn an_explicit_title_overrides_the_derived_one() {
    let store = MemorySessionStore::new();
    commit(&store, "s", 0, first_user_message("hello world")).expect("commit");
    commit(
        &store,
        "s",
        1,
        vec![SessionEntryKind::TitleSet {
            title: "login investigation".into(),
        }],
    )
    .expect("rename");
    let summaries = block_on(store.list_session_summaries()).expect("summaries");
    assert_eq!(summaries[0].title.as_deref(), Some("login investigation"));

    // The newest override wins.
    commit(
        &store,
        "s",
        2,
        vec![SessionEntryKind::TitleSet {
            title: "renamed again".into(),
        }],
    )
    .expect("rename twice");
    let summaries = block_on(store.list_session_summaries()).expect("summaries");
    assert_eq!(summaries[0].title.as_deref(), Some("renamed again"));
}

#[test]
fn invalid_titles_are_rejected_without_state_change() {
    let store = MemorySessionStore::new();
    for bad in ["   ", "line\nbreak", "nul\u{0}byte"] {
        let error = commit(
            &store,
            "s",
            0,
            vec![SessionEntryKind::TitleSet { title: bad.into() }],
        )
        .expect_err("invalid title");
        assert!(
            matches!(
                error,
                SessionError::Validation(SessionValidationError::InvalidTitle)
            ),
            "{bad:?}: {error:?}"
        );
    }
    let long = "x".repeat(201);
    let error = commit(
        &store,
        "s",
        0,
        vec![SessionEntryKind::TitleSet { title: long }],
    )
    .expect_err("too long");
    assert!(matches!(
        error,
        SessionError::Validation(SessionValidationError::InvalidTitle)
    ));
    assert!(block_on(store.list_sessions()).expect("list").is_empty());
}
