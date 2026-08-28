//! SESSION-008: 摘要投影与校验.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use philo_session::{
    ContextMessage, EntryId, MemorySessionStore, OperationId, OperationOutcome,
    SessionAssistantBlock, SessionEntry, SessionEntryKind, SessionError, SessionId,
    SessionProjection, SessionRevision, SessionStore, SessionTransaction, SessionUserPart,
    SessionValidationError, TurnId, TurnOutcome,
};

fn block_on<F: Future>(future: F) -> F::Output {
    struct ThreadWaker(std::thread::Thread);

    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}

fn session_id() -> SessionId {
    SessionId::new("m13")
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
                blocks: vec![SessionAssistantBlock::Text {
                    text: format!("assistant-{number}"),
                }],
            },
            SessionEntryKind::TurnTerminated {
                turn_id,
                outcome: TurnOutcome::Succeeded,
            },
            SessionEntryKind::OperationSettled {
                operation_id,
                outcome: OperationOutcome::Succeeded,
                usage: None,
            },
        ],
    )
}

fn open_turn(revision: SessionRevision, number: usize) -> SessionTransaction {
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
                operation_id,
                turn_id: turn_id.clone(),
            },
            SessionEntryKind::UserMessage {
                turn_id,
                parts: SessionUserPart::text_parts(format!("user-{number}")),
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

fn apply(projection: &mut SessionProjection, transaction: SessionTransaction) -> Vec<SessionEntry> {
    let applied = projection.apply(&transaction).expect("valid transaction");
    let entries = applied.entries().to_vec();
    *projection = applied.into_projection();
    entries
}

fn apply_turn(projection: &mut SessionProjection, number: usize) -> (Vec<SessionEntry>, EntryId) {
    let transaction = successful_turn(projection.revision(), number);
    let entries = apply(projection, transaction);
    let boundary = entries
        .last()
        .expect("successful turn settles its operation")
        .id()
        .clone();
    (entries, boundary)
}

fn apply_compaction(
    projection: &mut SessionProjection,
    summary: impl Into<String>,
    covers_up_to: EntryId,
) -> Vec<SessionEntry> {
    let transaction = compaction(projection.revision(), summary, covers_up_to);
    apply(projection, transaction)
}

fn apply_open_turn(projection: &mut SessionProjection, number: usize) -> Vec<SessionEntry> {
    let transaction = open_turn(projection.revision(), number);
    apply(projection, transaction)
}

fn expected_tail(from: usize, through: usize) -> Vec<ContextMessage> {
    (from..=through)
        .flat_map(|number| {
            [
                ContextMessage::User {
                    parts: SessionUserPart::text_parts(format!("user-{number}")),
                },
                ContextMessage::Assistant {
                    blocks: vec![SessionAssistantBlock::Text {
                        text: format!("assistant-{number}"),
                    }],
                },
            ]
        })
        .collect()
}

fn persisted_entries(
    projection: &SessionProjection,
    kinds: Vec<SessionEntryKind>,
) -> Vec<SessionEntry> {
    let mut parent = projection.current_leaf().cloned();
    kinds
        .into_iter()
        .enumerate()
        .map(|(index, kind)| {
            let id = EntryId::new(format!("forged-entry-{index}"));
            let entry = SessionEntry::from_persisted(id.clone(), parent.clone(), kind);
            parent = Some(id);
            entry
        })
        .collect()
}

fn assert_apply_and_replay_reject(
    projection: &SessionProjection,
    kinds: Vec<SessionEntryKind>,
    expected: SessionValidationError,
) {
    let apply_error = projection
        .apply(&SessionTransaction::linear(
            session_id(),
            projection.revision(),
            kinds.clone(),
        ))
        .expect_err("apply must reject malformed compaction");
    assert_eq!(apply_error, SessionError::Validation(expected.clone()));

    let mut replayed = projection.clone();
    let before = replayed.context_view(&session_id());
    let replay_error = replayed
        .replay(&persisted_entries(projection, kinds))
        .expect_err("replay must reject malformed compaction");
    assert_eq!(replay_error, SessionError::Validation(expected));
    assert_eq!(replayed.context_view(&session_id()), before);
}

#[test]
fn first_compaction_projects_summary_and_unmodified_tail() {
    let mut projection = SessionProjection::empty();
    let (first_entries, first_boundary) = apply_turn(&mut projection, 1);
    let original_first_entries = first_entries.clone();
    apply_turn(&mut projection, 2);
    apply_turn(&mut projection, 3);
    let previous_leaf = projection.current_leaf().cloned();

    let compacted = apply_compaction(&mut projection, "summary-1", first_boundary.clone());

    assert_eq!(compacted.len(), 1);
    assert_eq!(compacted[0].parent(), previous_leaf.as_ref());
    assert_eq!(
        compacted[0].kind(),
        &SessionEntryKind::Compaction {
            summary: "summary-1".to_owned(),
            covers_up_to: first_boundary,
        }
    );
    assert_eq!(first_entries, original_first_entries);

    let mut expected = vec![ContextMessage::Summary {
        text: "summary-1".to_owned(),
    }];
    expected.extend(expected_tail(2, 3));
    assert_eq!(projection.context_view(&session_id()).messages(), expected);
}

#[test]
fn second_compaction_replaces_the_previous_summary_and_advances_boundary() {
    let mut projection = SessionProjection::empty();
    let (_, first_boundary) = apply_turn(&mut projection, 1);
    let (_, second_boundary) = apply_turn(&mut projection, 2);
    let (_, third_boundary) = apply_turn(&mut projection, 3);

    apply_compaction(&mut projection, "summary-1", first_boundary.clone());
    apply_compaction(&mut projection, "summary-2", second_boundary.clone());

    let mut expected = vec![ContextMessage::Summary {
        text: "summary-2".to_owned(),
    }];
    expected.extend(expected_tail(3, 3));
    let view = projection.context_view(&session_id());
    assert_eq!(view.messages(), expected);
    assert_eq!(
        view.settled_turn_boundaries(),
        [first_boundary, second_boundary.clone(), third_boundary]
    );
    assert_eq!(view.latest_compaction_boundary(), Some(&second_boundary));
}

#[test]
fn compaction_keeps_open_turn_messages_and_lifecycle_state() {
    let mut projection = SessionProjection::empty();
    let (_, boundary) = apply_turn(&mut projection, 1);
    apply_open_turn(&mut projection, 2);

    apply_compaction(&mut projection, "settled prefix", boundary);

    let view = projection.context_view(&session_id());
    assert_eq!(
        view.messages(),
        [
            ContextMessage::Summary {
                text: "settled prefix".to_owned(),
            },
            ContextMessage::User {
                parts: SessionUserPart::text_parts("user-2"),
            },
        ]
    );
    assert_eq!(view.open_turns().len(), 1);
    assert_eq!(view.open_turns()[0].turn_id().as_str(), "turn-2");
}

#[test]
fn compaction_validation_matrix_is_identical_for_apply_and_replay() {
    let mut projection = SessionProjection::empty();
    let (first_entries, first_boundary) = apply_turn(&mut projection, 1);
    let (_, second_boundary) = apply_turn(&mut projection, 2);
    let user_entry = first_entries[2].id().clone();

    assert_apply_and_replay_reject(
        &projection,
        vec![SessionEntryKind::Compaction {
            summary: String::new(),
            covers_up_to: first_boundary.clone(),
        }],
        SessionValidationError::InvalidCompactionSummary,
    );
    assert_apply_and_replay_reject(
        &projection,
        vec![SessionEntryKind::Compaction {
            summary: "summary".to_owned(),
            covers_up_to: user_entry.clone(),
        }],
        SessionValidationError::InvalidCompactionBoundary {
            covers_up_to: user_entry,
        },
    );
    let missing = EntryId::new("missing-boundary");
    assert_apply_and_replay_reject(
        &projection,
        vec![SessionEntryKind::Compaction {
            summary: "summary".to_owned(),
            covers_up_to: missing.clone(),
        }],
        SessionValidationError::InvalidCompactionBoundary {
            covers_up_to: missing,
        },
    );
    assert_apply_and_replay_reject(
        &projection,
        vec![
            SessionEntryKind::Compaction {
                summary: "summary".to_owned(),
                covers_up_to: first_boundary.clone(),
            },
            SessionEntryKind::OperationStarted {
                operation_id: OperationId::new("mixed-operation"),
            },
        ],
        SessionValidationError::InvalidCompactionTransaction,
    );

    apply_compaction(&mut projection, "latest summary", second_boundary.clone());
    for boundary in [second_boundary.clone(), first_boundary] {
        assert_apply_and_replay_reject(
            &projection,
            vec![SessionEntryKind::Compaction {
                summary: "non-monotonic".to_owned(),
                covers_up_to: boundary.clone(),
            }],
            SessionValidationError::NonMonotonicCompactionBoundary {
                previous: second_boundary.clone(),
                covers_up_to: boundary,
            },
        );
    }
}

#[test]
fn valid_compaction_apply_and_replay_produce_the_same_projection() {
    let mut applied = SessionProjection::empty();
    let mut transactions = Vec::new();
    let (first_entries, first_boundary) = apply_turn(&mut applied, 1);
    transactions.push(first_entries);
    let (second_entries, _) = apply_turn(&mut applied, 2);
    transactions.push(second_entries);
    transactions.push(apply_compaction(
        &mut applied,
        "replayed summary",
        first_boundary,
    ));

    let mut replayed = SessionProjection::empty();
    for entries in &transactions {
        replayed.replay(entries).expect("valid facts replay");
    }

    assert_eq!(
        replayed.context_view(&session_id()),
        applied.context_view(&session_id())
    );
}

#[test]
fn memory_store_uses_the_shared_compaction_projection() {
    let store = MemorySessionStore::new();
    let first = block_on(store.commit(successful_turn(SessionRevision::ZERO, 1)))
        .expect("first turn commits");
    let boundary = first.entries().last().expect("turn settles").id().clone();
    block_on(store.commit(successful_turn(first.revision(), 2))).expect("second turn commits");
    block_on(store.commit(compaction(
        SessionRevision::new(2),
        "store summary",
        boundary,
    )))
    .expect("compaction commits");

    let view = block_on(store.context_view(&session_id())).expect("view");
    let mut expected = vec![ContextMessage::Summary {
        text: "store summary".to_owned(),
    }];
    expected.extend(expected_tail(2, 2));
    assert_eq!(view.messages(), expected);
}
