//! SESSION-005: `Cancelled` enum extensions and cancellation-transaction
//! validation in the shared core.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use philo_session::{
    CancelReason, ContextMessage, MemorySessionStore, OperationId, OperationOutcome,
    SessionAssistantBlock, SessionEntryKind, SessionError, SessionId, SessionProjection,
    SessionRevision, SessionStore, SessionToolCall, SessionToolResult, SessionTransaction,
    SessionUserPart, SessionValidationError, ToolBatchId, ToolCallId, ToolResultOutcome,
    TurnFailure, TurnFailureKind, TurnId, TurnOutcome,
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
    SessionId::new("cancel-session")
}

fn turn_id() -> TurnId {
    TurnId::new("turn-1")
}

fn operation_id() -> OperationId {
    OperationId::new("op-1")
}

fn start_transaction(revision: u64) -> SessionTransaction {
    SessionTransaction::linear(
        session_id(),
        SessionRevision::new(revision),
        vec![
            SessionEntryKind::OperationStarted {
                operation_id: operation_id(),
            },
            SessionEntryKind::TurnStarted {
                operation_id: operation_id(),
                turn_id: turn_id(),
            },
            SessionEntryKind::UserMessage {
                turn_id: turn_id(),
                parts: SessionUserPart::text_parts("hello"),
            },
        ],
    )
}

fn batch_transaction(revision: u64) -> SessionTransaction {
    SessionTransaction::linear(
        session_id(),
        SessionRevision::new(revision),
        vec![SessionEntryKind::AssistantToolCallBatch {
            turn_id: turn_id(),
            model_call_id: "model-call-1".to_owned(),
            tool_batch_id: ToolBatchId::new("batch-1"),
            blocks: vec![
                SessionAssistantBlock::ToolCall(SessionToolCall::new(
                    ToolCallId::new("call-1"),
                    "read",
                    "{}",
                )),
                SessionAssistantBlock::ToolCall(SessionToolCall::new(
                    ToolCallId::new("call-2"),
                    "read",
                    "{}",
                )),
            ],
        }],
    )
}

fn tool_result(result: SessionToolResult) -> SessionEntryKind {
    SessionEntryKind::ToolResult {
        turn_id: turn_id(),
        tool_batch_id: ToolBatchId::new("batch-1"),
        result,
    }
}

fn terminal_entries() -> Vec<SessionEntryKind> {
    vec![
        SessionEntryKind::TurnTerminated {
            turn_id: turn_id(),
            outcome: TurnOutcome::Cancelled {
                reason: CancelReason::User,
            },
        },
        SessionEntryKind::OperationSettled {
            operation_id: operation_id(),
            outcome: OperationOutcome::Cancelled {
                reason: CancelReason::User,
            },
            usage: None,
            generation: None,
        },
    ]
}

fn apply_all(
    projection: &mut SessionProjection,
    transactions: &[SessionTransaction],
) -> Result<(), SessionError> {
    for transaction in transactions {
        *projection = projection.apply(transaction)?.into_projection();
    }
    Ok(())
}

#[test]
fn partial_execution_cancel_commits_real_prefix_and_cancelled_suffix() {
    let mut projection = SessionProjection::empty();
    let mut cancel_entries = vec![
        tool_result(SessionToolResult::success(ToolCallId::new("call-1"), "ok")),
        tool_result(SessionToolResult::cancelled(ToolCallId::new("call-2"))),
    ];
    cancel_entries.extend(terminal_entries());
    apply_all(
        &mut projection,
        &[
            start_transaction(0),
            batch_transaction(1),
            SessionTransaction::linear(session_id(), SessionRevision::new(2), cancel_entries),
        ],
    )
    .expect("partial-execution cancellation is a valid transaction");
    assert_eq!(projection.revision(), SessionRevision::new(3));
}

#[test]
fn zero_execution_cancel_marks_the_whole_batch_cancelled() {
    let mut projection = SessionProjection::empty();
    let mut cancel_entries = vec![
        tool_result(SessionToolResult::cancelled(ToolCallId::new("call-1"))),
        tool_result(SessionToolResult::cancelled(ToolCallId::new("call-2"))),
    ];
    cancel_entries.extend(terminal_entries());
    apply_all(
        &mut projection,
        &[
            start_transaction(0),
            batch_transaction(1),
            SessionTransaction::linear(session_id(), SessionRevision::new(2), cancel_entries),
        ],
    )
    .expect("zero-execution cancellation is a valid transaction");
}

#[test]
fn model_stream_cancel_commits_terminal_entries_only() {
    let mut projection = SessionProjection::empty();
    apply_all(
        &mut projection,
        &[
            start_transaction(0),
            SessionTransaction::linear(session_id(), SessionRevision::new(1), terminal_entries()),
        ],
    )
    .expect("cancel without a batch needs only the two terminal entries");
}

#[test]
fn between_rounds_cancel_after_a_complete_batch_is_terminal_only() {
    let mut projection = SessionProjection::empty();
    apply_all(
        &mut projection,
        &[
            start_transaction(0),
            batch_transaction(1),
            SessionTransaction::linear(
                session_id(),
                SessionRevision::new(2),
                vec![
                    tool_result(SessionToolResult::success(ToolCallId::new("call-1"), "a")),
                    tool_result(SessionToolResult::success(ToolCallId::new("call-2"), "b")),
                ],
            ),
            SessionTransaction::linear(session_id(), SessionRevision::new(3), terminal_entries()),
        ],
    )
    .expect("between-rounds cancel adds no completion marks");
}

#[test]
fn session_continues_after_a_cancelled_turn_and_projects_cancelled_results() {
    let mut projection = SessionProjection::empty();
    let mut cancel_entries = vec![
        tool_result(SessionToolResult::success(ToolCallId::new("call-1"), "ok")),
        tool_result(SessionToolResult::cancelled(ToolCallId::new("call-2"))),
    ];
    cancel_entries.extend(terminal_entries());
    apply_all(
        &mut projection,
        &[
            start_transaction(0),
            batch_transaction(1),
            SessionTransaction::linear(session_id(), SessionRevision::new(2), cancel_entries),
            SessionTransaction::linear(
                session_id(),
                SessionRevision::new(3),
                vec![
                    SessionEntryKind::OperationStarted {
                        operation_id: OperationId::new("op-2"),
                    },
                    SessionEntryKind::TurnStarted {
                        operation_id: OperationId::new("op-2"),
                        turn_id: TurnId::new("turn-2"),
                    },
                    SessionEntryKind::UserMessage {
                        turn_id: TurnId::new("turn-2"),
                        parts: SessionUserPart::text_parts("next"),
                    },
                ],
            ),
        ],
    )
    .expect("a new turn starts normally after cancellation");
    assert_eq!(projection.revision(), SessionRevision::new(4));

    let view = projection.context_view(&session_id());
    let messages = view.messages();
    assert_eq!(messages.len(), 5);
    assert!(matches!(
        &messages[0],
        ContextMessage::User { parts } if parts == &SessionUserPart::text_parts("hello")
    ));
    assert!(matches!(
        &messages[1],
        ContextMessage::AssistantToolCalls { blocks, .. } if blocks.len() == 2
    ));
    assert!(matches!(
        &messages[2],
        ContextMessage::ToolResult { tool_call_id, outcome: ToolResultOutcome::Success { content } }
            if tool_call_id.as_str() == "call-1" && content == "ok"
    ));
    assert!(matches!(
        &messages[3],
        ContextMessage::ToolResult { tool_call_id, outcome: ToolResultOutcome::Cancelled }
            if tool_call_id.as_str() == "call-2"
    ));
    assert!(matches!(
        &messages[4],
        ContextMessage::User { parts } if parts == &SessionUserPart::text_parts("next")
    ));
}

#[test]
fn plain_result_commit_must_not_carry_cancelled_results() {
    let mut projection = SessionProjection::empty();
    apply_all(
        &mut projection,
        &[start_transaction(0), batch_transaction(1)],
    )
    .unwrap();

    let error = projection
        .apply(&SessionTransaction::linear(
            session_id(),
            SessionRevision::new(2),
            vec![
                tool_result(SessionToolResult::success(ToolCallId::new("call-1"), "ok")),
                tool_result(SessionToolResult::cancelled(ToolCallId::new("call-2"))),
            ],
        ))
        .err()
        .unwrap();
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidToolResult { turn_id: turn_id() })
    );
}

#[test]
fn cancelled_results_must_form_a_suffix() {
    let mut projection = SessionProjection::empty();
    apply_all(
        &mut projection,
        &[start_transaction(0), batch_transaction(1)],
    )
    .unwrap();

    let mut interleaved = vec![
        tool_result(SessionToolResult::cancelled(ToolCallId::new("call-1"))),
        tool_result(SessionToolResult::success(ToolCallId::new("call-2"), "ok")),
    ];
    interleaved.extend(terminal_entries());
    let error = projection
        .apply(&SessionTransaction::linear(
            session_id(),
            SessionRevision::new(2),
            interleaved,
        ))
        .err()
        .unwrap();
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidToolResult { turn_id: turn_id() })
    );
}

#[test]
fn cancel_terminal_requires_the_newest_batch_to_be_resolved() {
    let mut projection = SessionProjection::empty();
    apply_all(
        &mut projection,
        &[start_transaction(0), batch_transaction(1)],
    )
    .unwrap();

    let error = projection
        .apply(&SessionTransaction::linear(
            session_id(),
            SessionRevision::new(2),
            terminal_entries(),
        ))
        .err()
        .unwrap();
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidTurnOutcome { turn_id: turn_id() })
    );
}

#[test]
fn incomplete_completion_marks_are_rejected() {
    let mut projection = SessionProjection::empty();
    apply_all(
        &mut projection,
        &[start_transaction(0), batch_transaction(1)],
    )
    .unwrap();

    // Only one of two calls resolved: the batch is still incomplete.
    let mut partial = vec![tool_result(SessionToolResult::cancelled(ToolCallId::new(
        "call-1",
    )))];
    partial.extend(terminal_entries());
    let error = projection
        .apply(&SessionTransaction::linear(
            session_id(),
            SessionRevision::new(2),
            partial,
        ))
        .err()
        .unwrap();
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidToolResult { turn_id: turn_id() })
    );
}

#[test]
fn a_turn_with_an_assistant_message_cannot_cancel() {
    let mut projection = SessionProjection::empty();
    apply_all(
        &mut projection,
        &[
            start_transaction(0),
            SessionTransaction::linear(
                session_id(),
                SessionRevision::new(1),
                vec![SessionEntryKind::AssistantMessage {
                    turn_id: turn_id(),
                    blocks: vec![SessionAssistantBlock::Text {
                        text: "done".into(),
                    }],
                }],
            ),
        ],
    )
    .unwrap();

    let error = projection
        .apply(&SessionTransaction::linear(
            session_id(),
            SessionRevision::new(2),
            terminal_entries(),
        ))
        .err()
        .unwrap();
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidTurnOutcome { turn_id: turn_id() })
    );
}

#[test]
fn a_turn_with_a_failure_cannot_cancel() {
    let mut projection = SessionProjection::empty();
    apply_all(
        &mut projection,
        &[
            start_transaction(0),
            SessionTransaction::linear(
                session_id(),
                SessionRevision::new(1),
                vec![SessionEntryKind::TurnFailure {
                    turn_id: turn_id(),
                    failure: TurnFailure::new(TurnFailureKind::ModelCall, "offline"),
                }],
            ),
        ],
    )
    .unwrap();

    let error = projection
        .apply(&SessionTransaction::linear(
            session_id(),
            SessionRevision::new(2),
            terminal_entries(),
        ))
        .err()
        .unwrap();
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidTurnOutcome { turn_id: turn_id() })
    );
}

#[test]
fn operation_outcome_must_match_the_cancelled_turn() {
    let mut projection = SessionProjection::empty();
    apply_all(&mut projection, &[start_transaction(0)]).unwrap();

    let error = projection
        .apply(&SessionTransaction::linear(
            session_id(),
            SessionRevision::new(1),
            vec![
                SessionEntryKind::TurnTerminated {
                    turn_id: turn_id(),
                    outcome: TurnOutcome::Cancelled {
                        reason: CancelReason::User,
                    },
                },
                SessionEntryKind::OperationSettled {
                    operation_id: operation_id(),
                    outcome: OperationOutcome::Failed,
                usage: None,
                generation: None,
                },
            ],
        ))
        .err()
        .unwrap();
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidOperationOutcome {
            operation_id: operation_id()
        })
    );
}

#[test]
fn cancelled_operation_outcome_requires_a_cancelled_turn() {
    let mut projection = SessionProjection::empty();
    apply_all(&mut projection, &[start_transaction(0)]).unwrap();

    let error = projection
        .apply(&SessionTransaction::linear(
            session_id(),
            SessionRevision::new(1),
            vec![
                SessionEntryKind::AssistantMessage {
                    turn_id: turn_id(),
                    blocks: vec![SessionAssistantBlock::Text {
                        text: "done".into(),
                    }],
                },
                SessionEntryKind::TurnTerminated {
                    turn_id: turn_id(),
                    outcome: TurnOutcome::Succeeded,
                },
                SessionEntryKind::OperationSettled {
                    operation_id: operation_id(),
                    outcome: OperationOutcome::Cancelled {
                        reason: CancelReason::User,
                    },
                    usage: None,
                    generation: None,
                },
            ],
        ))
        .err()
        .unwrap();
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidOperationOutcome {
            operation_id: operation_id()
        })
    );
}

#[test]
fn apply_and_replay_agree_on_cancellation_transactions() {
    let mut cancel_entries = vec![
        tool_result(SessionToolResult::success(ToolCallId::new("call-1"), "ok")),
        tool_result(SessionToolResult::cancelled(ToolCallId::new("call-2"))),
    ];
    cancel_entries.extend(terminal_entries());
    let transactions = [
        start_transaction(0),
        batch_transaction(1),
        SessionTransaction::linear(session_id(), SessionRevision::new(2), cancel_entries),
    ];

    let mut applied = SessionProjection::empty();
    let mut committed = Vec::new();
    for transaction in &transactions {
        let result = applied.apply(transaction).expect("apply");
        committed.push(result.entries().to_vec());
        applied = result.into_projection();
    }

    let mut replayed = SessionProjection::empty();
    for entries in &committed {
        replayed.replay(entries).expect("replay committed facts");
    }
    assert_eq!(replayed.revision(), applied.revision());
    assert_eq!(replayed.current_leaf(), applied.current_leaf());
    assert_eq!(
        replayed.context_view(&session_id()),
        applied.context_view(&session_id())
    );
}

#[test]
fn memory_store_accepts_cancellation_through_the_shared_core() {
    let store = MemorySessionStore::new();
    let mut cancel_entries = vec![
        tool_result(SessionToolResult::success(ToolCallId::new("call-1"), "ok")),
        tool_result(SessionToolResult::cancelled(ToolCallId::new("call-2"))),
    ];
    cancel_entries.extend(terminal_entries());
    for transaction in [
        start_transaction(0),
        batch_transaction(1),
        SessionTransaction::linear(session_id(), SessionRevision::new(2), cancel_entries),
    ] {
        block_on(store.commit(transaction)).expect("memory store commit");
    }
    let view = block_on(store.context_view(&session_id())).expect("view");
    assert_eq!(view.revision(), SessionRevision::new(3));
    assert!(view.messages().iter().any(|message| matches!(
        message,
        ContextMessage::ToolResult {
            outcome: ToolResultOutcome::Cancelled,
            ..
        }
    )));
}
