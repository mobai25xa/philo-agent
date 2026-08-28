//! SESSION-001: MemorySession 原子提交与空上下文.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use philo_session::SessionEntryKind::{
    AssistantMessage, OperationSettled, OperationStarted, TurnFailure as TurnFailureEntry,
    TurnStarted, TurnTerminated, UserMessage,
};
use philo_session::{
    ContextMessage, MemorySessionStore, NewSessionEntry, OperationId, OperationOutcome,
    SessionAssistantBlock, SessionError, SessionId, SessionRevision, SessionStore,
    SessionTransaction, SessionUserPart, SessionValidationError, TurnFailure, TurnFailureKind,
    TurnId, TurnOutcome,
};

fn block_on<F: Future>(future: F) -> F::Output {
    let mut context = Context::from_waker(Waker::noop());
    let mut future = pin!(future);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("memory store future unexpectedly pending"),
    }
}

fn ids(number: usize) -> (OperationId, TurnId) {
    (
        OperationId::new(format!("operation-{number}")),
        TurnId::new(format!("turn-{number}")),
    )
}

fn start_transaction(
    session_id: &SessionId,
    revision: SessionRevision,
    operation_id: &OperationId,
    turn_id: &TurnId,
    content: &str,
) -> SessionTransaction {
    SessionTransaction::linear(
        session_id.clone(),
        revision,
        vec![
            OperationStarted {
                operation_id: operation_id.clone(),
            },
            TurnStarted {
                operation_id: operation_id.clone(),
                turn_id: turn_id.clone(),
            },
            UserMessage {
                turn_id: turn_id.clone(),
                parts: SessionUserPart::text_parts(content),
            },
        ],
    )
}

fn success_transaction(
    session_id: &SessionId,
    revision: SessionRevision,
    operation_id: &OperationId,
    turn_id: &TurnId,
    content: &str,
) -> SessionTransaction {
    SessionTransaction::linear(
        session_id.clone(),
        revision,
        vec![
            AssistantMessage {
                turn_id: turn_id.clone(),
                blocks: vec![SessionAssistantBlock::Text {
                    text: content.to_owned(),
                }],
            },
            TurnTerminated {
                turn_id: turn_id.clone(),
                outcome: TurnOutcome::Succeeded,
            },
            OperationSettled {
                operation_id: operation_id.clone(),
                outcome: OperationOutcome::Succeeded,
                usage: None,
            },
        ],
    )
}

fn failure_transaction(
    session_id: &SessionId,
    revision: SessionRevision,
    operation_id: &OperationId,
    turn_id: &TurnId,
) -> SessionTransaction {
    SessionTransaction::linear(
        session_id.clone(),
        revision,
        vec![
            TurnFailureEntry {
                turn_id: turn_id.clone(),
                failure: TurnFailure::new(TurnFailureKind::ModelCall, "offline"),
            },
            TurnTerminated {
                turn_id: turn_id.clone(),
                outcome: TurnOutcome::Failed,
            },
            OperationSettled {
                operation_id: operation_id.clone(),
                outcome: OperationOutcome::Failed,
                usage: None,
            },
        ],
    )
}

#[test]
fn empty_context() {
    let store = MemorySessionStore::new();
    let session_id = SessionId::new("session");
    let view = block_on(store.context_view(&session_id)).unwrap();
    assert_eq!(view.revision(), SessionRevision::ZERO);
    assert_eq!(view.current_leaf(), None);
    assert!(view.messages().is_empty());
}

#[test]
fn atomic_turn_start_commit() {
    let store = MemorySessionStore::new();
    let session_id = SessionId::new("session");
    let (operation_id, turn_id) = ids(1);
    let commit = block_on(store.commit(start_transaction(
        &session_id,
        SessionRevision::ZERO,
        &operation_id,
        &turn_id,
        "hi",
    )))
    .unwrap();
    assert_eq!(commit.revision(), SessionRevision::new(1));
    assert_eq!(commit.entries().len(), 3);
    assert_eq!(commit.current_leaf(), commit.entries()[2].id());
    assert_eq!(commit.entries()[1].parent(), Some(commit.entries()[0].id()));
    assert_eq!(commit.entries()[2].parent(), Some(commit.entries()[1].id()));

    let view = block_on(store.context_view(&session_id)).unwrap();
    assert_eq!(
        view.messages(),
        &[ContextMessage::User {
            parts: SessionUserPart::text_parts("hi")
        }]
    );
}

#[test]
fn atomic_turn_success_commit() {
    let store = MemorySessionStore::new();
    let session_id = SessionId::new("session");
    let (operation_id, turn_id) = ids(1);
    let started = block_on(store.commit(start_transaction(
        &session_id,
        SessionRevision::ZERO,
        &operation_id,
        &turn_id,
        "hi",
    )))
    .unwrap();
    let completed = block_on(store.commit(success_transaction(
        &session_id,
        started.revision(),
        &operation_id,
        &turn_id,
        "hello",
    )))
    .unwrap();
    assert_eq!(completed.revision(), SessionRevision::new(2));
    assert_eq!(
        completed.entries()[0].parent(),
        Some(started.current_leaf())
    );
    assert_eq!(completed.current_leaf(), completed.entries()[2].id());
}

#[test]
fn revision_conflict_has_no_partial_write() {
    let store = MemorySessionStore::new();
    let session_id = SessionId::new("session");
    let (operation_id, turn_id) = ids(1);
    let started = block_on(store.commit(start_transaction(
        &session_id,
        SessionRevision::ZERO,
        &operation_id,
        &turn_id,
        "hi",
    )))
    .unwrap();
    let before = block_on(store.context_view(&session_id)).unwrap();
    let error = block_on(store.commit(success_transaction(
        &session_id,
        SessionRevision::ZERO,
        &operation_id,
        &turn_id,
        "must not appear",
    )))
    .unwrap_err();
    assert_eq!(
        error,
        SessionError::RevisionConflict {
            expected: SessionRevision::ZERO,
            actual: started.revision(),
        }
    );
    assert_eq!(block_on(store.context_view(&session_id)).unwrap(), before);
}

#[test]
fn invalid_entry_relation_rejected() {
    let store = MemorySessionStore::new();
    let session_id = SessionId::new("session");
    let (_, turn_id) = ids(1);
    let transaction = SessionTransaction::new(
        session_id.clone(),
        SessionRevision::ZERO,
        vec![NewSessionEntry::at_current_leaf(UserMessage {
            turn_id: turn_id.clone(),
            parts: SessionUserPart::text_parts("orphan"),
        })],
        0,
    );
    let error = block_on(store.commit(transaction)).unwrap_err();
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidTurnReference { turn_id })
    );
    assert_eq!(
        block_on(store.context_view(&session_id))
            .unwrap()
            .revision(),
        SessionRevision::ZERO
    );
}

#[test]
fn duplicate_turn_termination_rejected() {
    let store = MemorySessionStore::new();
    let session_id = SessionId::new("session");
    let (operation_id, turn_id) = ids(1);
    block_on(store.commit(start_transaction(
        &session_id,
        SessionRevision::ZERO,
        &operation_id,
        &turn_id,
        "hi",
    )))
    .unwrap();
    block_on(store.commit(success_transaction(
        &session_id,
        SessionRevision::new(1),
        &operation_id,
        &turn_id,
        "hello",
    )))
    .unwrap();
    let error = block_on(store.commit(SessionTransaction::linear(
        session_id,
        SessionRevision::new(2),
        vec![TurnTerminated {
            turn_id: turn_id.clone(),
            outcome: TurnOutcome::Succeeded,
        }],
    )))
    .unwrap_err();
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidTurnReference { turn_id })
    );
}

#[test]
fn two_completed_turns_project_ordered_messages() {
    let store = MemorySessionStore::new();
    let session_id = SessionId::new("session");
    for (number, user, assistant) in [(1, "one", "first"), (2, "two", "second")] {
        let (operation_id, turn_id) = ids(number);
        let revision = block_on(store.context_view(&session_id))
            .unwrap()
            .revision();
        let started = block_on(store.commit(start_transaction(
            &session_id,
            revision,
            &operation_id,
            &turn_id,
            user,
        )))
        .unwrap();
        block_on(store.commit(success_transaction(
            &session_id,
            started.revision(),
            &operation_id,
            &turn_id,
            assistant,
        )))
        .unwrap();
    }
    assert_eq!(
        block_on(store.context_view(&session_id))
            .unwrap()
            .messages(),
        &[
            ContextMessage::User {
                parts: SessionUserPart::text_parts("one"),
            },
            ContextMessage::Assistant {
                blocks: vec![SessionAssistantBlock::Text {
                    text: "first".to_owned(),
                }],
            },
            ContextMessage::User {
                parts: SessionUserPart::text_parts("two"),
            },
            ContextMessage::Assistant {
                blocks: vec![SessionAssistantBlock::Text {
                    text: "second".to_owned(),
                }],
            },
        ]
    );
}

#[test]
fn atomic_turn_failure_commit() {
    let store = MemorySessionStore::new();
    let session_id = SessionId::new("session");
    let (operation_id, turn_id) = ids(1);
    block_on(store.commit(start_transaction(
        &session_id,
        SessionRevision::ZERO,
        &operation_id,
        &turn_id,
        "hi",
    )))
    .unwrap();
    let failed = block_on(store.commit(failure_transaction(
        &session_id,
        SessionRevision::new(1),
        &operation_id,
        &turn_id,
    )))
    .unwrap();
    assert_eq!(failed.revision(), SessionRevision::new(2));
    assert_eq!(failed.entries().len(), 3);
    assert_eq!(failed.current_leaf(), failed.entries()[2].id());
    assert_eq!(
        block_on(store.context_view(&session_id))
            .unwrap()
            .messages(),
        &[ContextMessage::User {
            parts: SessionUserPart::text_parts("hi")
        }]
    );
}

#[test]
fn failure_cannot_terminate_an_already_terminal_turn() {
    let store = MemorySessionStore::new();
    let session_id = SessionId::new("session");
    let (operation_id, turn_id) = ids(1);
    block_on(store.commit(start_transaction(
        &session_id,
        SessionRevision::ZERO,
        &operation_id,
        &turn_id,
        "hi",
    )))
    .unwrap();
    block_on(store.commit(success_transaction(
        &session_id,
        SessionRevision::new(1),
        &operation_id,
        &turn_id,
        "hello",
    )))
    .unwrap();
    let before = block_on(store.context_view(&session_id)).unwrap();
    let error = block_on(store.commit(failure_transaction(
        &session_id,
        SessionRevision::new(2),
        &operation_id,
        &turn_id,
    )))
    .unwrap_err();
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidTurnReference { turn_id })
    );
    assert_eq!(block_on(store.context_view(&session_id)).unwrap(), before);
}

#[test]
fn list_sessions_empty_store_is_empty() {
    let store = MemorySessionStore::new();
    assert!(block_on(store.list_sessions()).unwrap().is_empty());
}

#[test]
fn list_sessions_returns_known_ids() {
    let store = MemorySessionStore::new();
    let first = SessionId::new("alpha");
    let second = SessionId::new("beta");
    let (operation_id, turn_id) = ids(1);
    block_on(store.commit(start_transaction(
        &first,
        SessionRevision::ZERO,
        &operation_id,
        &turn_id,
        "hi",
    )))
    .unwrap();
    let (operation_id, turn_id) = ids(2);
    block_on(store.commit(start_transaction(
        &second,
        SessionRevision::ZERO,
        &operation_id,
        &turn_id,
        "hey",
    )))
    .unwrap();
    let unknown = SessionId::new("ghost");
    block_on(store.context_view(&unknown)).unwrap();

    let mut listed = block_on(store.list_sessions()).unwrap();
    listed.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    assert_eq!(
        listed.iter().map(SessionId::as_str).collect::<Vec<_>>(),
        ["alpha", "beta"]
    );
}
