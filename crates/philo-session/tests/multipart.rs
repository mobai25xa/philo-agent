//! SESSION-006: multi-part user message entries, context projection, and
//! the shared core's structural parts rules.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use philo_session::{
    ContextMessage, MemorySessionStore, OperationId, SessionEntryKind, SessionError, SessionId,
    SessionProjection, SessionRevision, SessionStore, SessionTransaction, SessionUserPart,
    SessionValidationError, TurnId,
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

fn session_id() -> SessionId {
    SessionId::new("multipart-session")
}

fn turn_id() -> TurnId {
    TurnId::new("turn-1")
}

fn png_bytes() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0xFF, 0x00, 0x7F,
    ]
}

fn mixed_parts() -> Vec<SessionUserPart> {
    vec![
        SessionUserPart::Text("what is in this picture?".to_owned()),
        SessionUserPart::Image {
            media_type: "image/png".to_owned(),
            bytes: png_bytes(),
        },
    ]
}

fn start_transaction(parts: Vec<SessionUserPart>) -> SessionTransaction {
    SessionTransaction::linear(
        session_id(),
        SessionRevision::ZERO,
        vec![
            SessionEntryKind::OperationStarted {
                operation_id: OperationId::new("op-1"),
            },
            SessionEntryKind::TurnStarted {
                operation_id: OperationId::new("op-1"),
                turn_id: turn_id(),
            },
            SessionEntryKind::UserMessage {
                turn_id: turn_id(),
                parts,
            },
        ],
    )
}

#[test]
fn committed_image_parts_project_byte_for_byte() {
    let store = MemorySessionStore::new();
    block_on(store.commit(start_transaction(mixed_parts()))).unwrap();

    let view = block_on(store.context_view(&session_id())).unwrap();
    assert_eq!(
        view.messages(),
        &[ContextMessage::User {
            parts: mixed_parts()
        }]
    );
}

#[test]
fn image_only_message_is_valid() {
    let store = MemorySessionStore::new();
    let parts = vec![SessionUserPart::Image {
        media_type: "image/jpeg".to_owned(),
        bytes: png_bytes(),
    }];
    block_on(store.commit(start_transaction(parts.clone()))).unwrap();

    let view = block_on(store.context_view(&session_id())).unwrap();
    assert_eq!(view.messages(), &[ContextMessage::User { parts }]);
}

#[test]
fn replay_reproduces_image_parts_exactly() {
    let applied = SessionProjection::empty()
        .apply(&start_transaction(mixed_parts()))
        .expect("valid transaction applies");

    let mut replayed = SessionProjection::empty();
    replayed
        .replay(applied.entries())
        .expect("committed entries replay");

    assert_eq!(
        replayed.context_view(&session_id()),
        applied.projection().context_view(&session_id()),
    );
    assert_eq!(
        replayed.context_view(&session_id()).messages(),
        &[ContextMessage::User {
            parts: mixed_parts()
        }]
    );
}

#[test]
fn empty_parts_are_rejected() {
    let error = SessionProjection::empty()
        .apply(&start_transaction(Vec::new()))
        .expect_err("empty parts must be rejected");
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidUserMessage { turn_id: turn_id() })
    );
}

#[test]
fn empty_text_part_is_rejected() {
    let parts = vec![
        SessionUserPart::Text(String::new()),
        SessionUserPart::Image {
            media_type: "image/png".to_owned(),
            bytes: png_bytes(),
        },
    ];
    let error = SessionProjection::empty()
        .apply(&start_transaction(parts))
        .expect_err("empty text part must be rejected");
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidUserMessage { turn_id: turn_id() })
    );
}

#[test]
fn empty_media_type_is_rejected() {
    let parts = vec![SessionUserPart::Image {
        media_type: String::new(),
        bytes: png_bytes(),
    }];
    let error = SessionProjection::empty()
        .apply(&start_transaction(parts))
        .expect_err("empty media type must be rejected");
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidUserMessage { turn_id: turn_id() })
    );
}

#[test]
fn rejected_parts_leave_no_partial_write() {
    let store = MemorySessionStore::new();
    let error = block_on(store.commit(start_transaction(Vec::new()))).unwrap_err();
    assert!(matches!(
        error,
        SessionError::Validation(SessionValidationError::InvalidUserMessage { .. })
    ));

    let view = block_on(store.context_view(&session_id())).unwrap();
    assert_eq!(view.revision(), SessionRevision::ZERO);
    assert!(view.messages().is_empty());
}

#[test]
fn replay_applies_the_same_structural_rules() {
    // Forge a committed-looking entry stream with malformed parts: replay
    // must reject it exactly like apply would.
    let valid = SessionProjection::empty()
        .apply(&start_transaction(mixed_parts()))
        .unwrap();
    let forged = valid
        .entries()
        .iter()
        .map(|entry| {
            let kind = match entry.kind() {
                SessionEntryKind::UserMessage { turn_id, .. } => SessionEntryKind::UserMessage {
                    turn_id: turn_id.clone(),
                    parts: Vec::new(),
                },
                other => other.clone(),
            };
            philo_session::SessionEntry::from_persisted(
                entry.id().clone(),
                entry.parent().cloned(),
                kind,
            )
        })
        .collect::<Vec<_>>();

    let mut projection = SessionProjection::empty();
    let error = projection
        .replay(&forged)
        .expect_err("forged empty parts must not replay");
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidUserMessage { turn_id: turn_id() })
    );
}
