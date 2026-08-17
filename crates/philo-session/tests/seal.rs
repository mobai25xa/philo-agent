//! SESSION-007: cancel reasons, `Interrupted` completion marks, seal
//! transaction validation, and the `open_turns` query surface.

use philo_session::{
    CancelReason, MemorySessionStore, OperationId, OperationOutcome, SessionAssistantBlock,
    SessionEntryKind, SessionError, SessionId, SessionProjection, SessionRevision, SessionStore,
    SessionToolCall, SessionToolResult, SessionTransaction, SessionUserPart,
    SessionValidationError, ToolBatchId, ToolCallId, TurnId, TurnOutcome,
};

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

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
    SessionId::new("seal-session")
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
                    "write",
                    "{}",
                )),
                SessionAssistantBlock::ToolCall(SessionToolCall::new(
                    ToolCallId::new("call-2"),
                    "shell",
                    "{}",
                )),
            ],
        }],
    )
}

fn three_call_batch_transaction(revision: u64) -> SessionTransaction {
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
                    "write",
                    "{}",
                )),
                SessionAssistantBlock::ToolCall(SessionToolCall::new(
                    ToolCallId::new("call-2"),
                    "shell",
                    "{}",
                )),
                SessionAssistantBlock::ToolCall(SessionToolCall::new(
                    ToolCallId::new("call-3"),
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

fn terminal_entries(reason: CancelReason) -> Vec<SessionEntryKind> {
    vec![
        SessionEntryKind::TurnTerminated {
            turn_id: turn_id(),
            outcome: TurnOutcome::Cancelled { reason },
        },
        SessionEntryKind::OperationSettled {
            operation_id: operation_id(),
            outcome: OperationOutcome::Cancelled { reason },
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

fn invalid_tool_result() -> SessionError {
    SessionError::Validation(SessionValidationError::InvalidToolResult { turn_id: turn_id() })
}

// ---------------------------------------------------------------- seal 合法形态

#[test]
fn seal_completes_the_stranded_batch_with_interrupted_marks() {
    let mut projection = SessionProjection::empty();
    let mut seal = vec![
        tool_result(SessionToolResult::interrupted(ToolCallId::new("call-1"))),
        tool_result(SessionToolResult::interrupted(ToolCallId::new("call-2"))),
    ];
    seal.extend(terminal_entries(CancelReason::Abandoned));
    apply_all(
        &mut projection,
        &[
            start_transaction(0),
            batch_transaction(1),
            SessionTransaction::linear(session_id(), SessionRevision::new(2), seal),
        ],
    )
    .expect("a seal transaction completes the batch with interrupted marks");
    assert_eq!(projection.revision(), SessionRevision::new(3));
    assert!(
        projection
            .context_view(&session_id())
            .open_turns()
            .is_empty()
    );
}

#[test]
fn seal_without_a_stranded_batch_is_terminal_entries_only() {
    let mut projection = SessionProjection::empty();
    apply_all(
        &mut projection,
        &[
            start_transaction(0),
            SessionTransaction::linear(
                session_id(),
                SessionRevision::new(1),
                terminal_entries(CancelReason::Abandoned),
            ),
        ],
    )
    .expect("a model-stream remnant seals with the two terminal entries");
    assert!(
        projection
            .context_view(&session_id())
            .open_turns()
            .is_empty()
    );
}

#[test]
fn timeout_cancellation_keeps_the_cancelled_prefix_shape() {
    let mut projection = SessionProjection::empty();
    let mut cancel = vec![
        tool_result(SessionToolResult::success(ToolCallId::new("call-1"), "ok")),
        tool_result(SessionToolResult::cancelled(ToolCallId::new("call-2"))),
    ];
    cancel.extend(terminal_entries(CancelReason::Timeout));
    apply_all(
        &mut projection,
        &[
            start_transaction(0),
            batch_transaction(1),
            SessionTransaction::linear(session_id(), SessionRevision::new(2), cancel),
        ],
    )
    .expect("a timeout cancellation reuses the real-prefix + cancelled-suffix shape");
}

// ---------------------------------------------------------------- 校验矩阵

#[test]
fn interrupted_marks_are_allowed_in_user_cancellations() {
    let mut projection = SessionProjection::empty();
    let mut cancel = vec![
        tool_result(SessionToolResult::interrupted(ToolCallId::new("call-1"))),
        tool_result(SessionToolResult::interrupted(ToolCallId::new("call-2"))),
    ];
    cancel.extend(terminal_entries(CancelReason::User));
    apply_all(
        &mut projection,
        &[
            start_transaction(0),
            batch_transaction(1),
            SessionTransaction::linear(session_id(), SessionRevision::new(2), cancel),
        ],
    )
    .expect("all-interrupted user cancellation is a valid transaction");
}

#[test]
fn user_cancellation_accepts_success_and_interrupted() {
    let mut projection = SessionProjection::empty();
    let mut cancel = vec![
        tool_result(SessionToolResult::success(ToolCallId::new("call-1"), "ok")),
        tool_result(SessionToolResult::interrupted(ToolCallId::new("call-2"))),
    ];
    cancel.extend(terminal_entries(CancelReason::User));
    apply_all(
        &mut projection,
        &[
            start_transaction(0),
            batch_transaction(1),
            SessionTransaction::linear(session_id(), SessionRevision::new(2), cancel),
        ],
    )
    .expect("success then interrupted is a valid user cancellation");
}

#[test]
fn user_cancellation_accepts_interrupted_and_cancelled() {
    let mut projection = SessionProjection::empty();
    let mut cancel = vec![
        tool_result(SessionToolResult::interrupted(ToolCallId::new("call-1"))),
        tool_result(SessionToolResult::cancelled(ToolCallId::new("call-2"))),
    ];
    cancel.extend(terminal_entries(CancelReason::User));
    apply_all(
        &mut projection,
        &[
            start_transaction(0),
            batch_transaction(1),
            SessionTransaction::linear(session_id(), SessionRevision::new(2), cancel),
        ],
    )
    .expect("interrupted then cancelled is a valid user cancellation");
}

#[test]
fn user_cancellation_accepts_success_interrupted_cancelled_suffix() {
    let mut projection = SessionProjection::empty();
    let mut cancel = vec![
        tool_result(SessionToolResult::success(ToolCallId::new("call-1"), "ok")),
        tool_result(SessionToolResult::interrupted(ToolCallId::new("call-2"))),
        tool_result(SessionToolResult::cancelled(ToolCallId::new("call-3"))),
    ];
    cancel.extend(terminal_entries(CancelReason::User));
    apply_all(
        &mut projection,
        &[
            start_transaction(0),
            three_call_batch_transaction(1),
            SessionTransaction::linear(session_id(), SessionRevision::new(2), cancel),
        ],
    )
    .expect("real then interrupted then cancelled suffix is a valid user cancellation");
}

#[test]
fn interrupted_after_cancelled_is_rejected_in_user_cancellations() {
    let mut projection = SessionProjection::empty();
    apply_all(
        &mut projection,
        &[start_transaction(0), batch_transaction(1)],
    )
    .unwrap();

    let mut cancel = vec![
        tool_result(SessionToolResult::cancelled(ToolCallId::new("call-1"))),
        tool_result(SessionToolResult::interrupted(ToolCallId::new("call-2"))),
    ];
    cancel.extend(terminal_entries(CancelReason::User));
    let error = projection
        .apply(&SessionTransaction::linear(
            session_id(),
            SessionRevision::new(2),
            cancel,
        ))
        .err()
        .unwrap();
    assert_eq!(error, invalid_tool_result());
}

#[test]
fn timeout_cancellation_accepts_interrupted_and_cancelled() {
    let mut projection = SessionProjection::empty();
    let mut cancel = vec![
        tool_result(SessionToolResult::interrupted(ToolCallId::new("call-1"))),
        tool_result(SessionToolResult::cancelled(ToolCallId::new("call-2"))),
    ];
    cancel.extend(terminal_entries(CancelReason::Timeout));
    apply_all(
        &mut projection,
        &[
            start_transaction(0),
            batch_transaction(1),
            SessionTransaction::linear(session_id(), SessionRevision::new(2), cancel),
        ],
    )
    .expect("interrupted then cancelled is a valid timeout cancellation");
}

#[test]
fn interrupted_marks_are_rejected_in_plain_result_commits() {
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
                tool_result(SessionToolResult::interrupted(ToolCallId::new("call-2"))),
            ],
        ))
        .err()
        .unwrap();
    assert_eq!(error, invalid_tool_result());
}

#[test]
fn seal_completion_must_not_carry_real_results() {
    let mut projection = SessionProjection::empty();
    apply_all(
        &mut projection,
        &[start_transaction(0), batch_transaction(1)],
    )
    .unwrap();

    let mut seal = vec![
        tool_result(SessionToolResult::success(ToolCallId::new("call-1"), "ok")),
        tool_result(SessionToolResult::interrupted(ToolCallId::new("call-2"))),
    ];
    seal.extend(terminal_entries(CancelReason::Abandoned));
    let error = projection
        .apply(&SessionTransaction::linear(
            session_id(),
            SessionRevision::new(2),
            seal,
        ))
        .err()
        .unwrap();
    assert_eq!(error, invalid_tool_result());
}

#[test]
fn seal_completion_must_not_carry_cancelled_marks() {
    let mut projection = SessionProjection::empty();
    apply_all(
        &mut projection,
        &[start_transaction(0), batch_transaction(1)],
    )
    .unwrap();

    let mut seal = vec![
        tool_result(SessionToolResult::interrupted(ToolCallId::new("call-1"))),
        tool_result(SessionToolResult::cancelled(ToolCallId::new("call-2"))),
    ];
    seal.extend(terminal_entries(CancelReason::Abandoned));
    let error = projection
        .apply(&SessionTransaction::linear(
            session_id(),
            SessionRevision::new(2),
            seal,
        ))
        .err()
        .unwrap();
    assert_eq!(error, invalid_tool_result());
}

#[test]
fn cancelled_marks_are_rejected_in_abandoned_seals_even_alone() {
    let mut projection = SessionProjection::empty();
    apply_all(
        &mut projection,
        &[start_transaction(0), batch_transaction(1)],
    )
    .unwrap();

    let mut seal = vec![
        tool_result(SessionToolResult::cancelled(ToolCallId::new("call-1"))),
        tool_result(SessionToolResult::cancelled(ToolCallId::new("call-2"))),
    ];
    seal.extend(terminal_entries(CancelReason::Abandoned));
    let error = projection
        .apply(&SessionTransaction::linear(
            session_id(),
            SessionRevision::new(2),
            seal,
        ))
        .err()
        .unwrap();
    assert_eq!(error, invalid_tool_result());
}

#[test]
fn turn_and_operation_reasons_must_agree() {
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
                        reason: CancelReason::Abandoned,
                    },
                },
                SessionEntryKind::OperationSettled {
                    operation_id: operation_id(),
                    outcome: OperationOutcome::Cancelled {
                        reason: CancelReason::User,
                    },
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

// ---------------------------------------------------------------- open_turns

#[test]
fn open_turns_reports_the_stranded_batch_suffix() {
    let mut projection = SessionProjection::empty();
    apply_all(
        &mut projection,
        &[start_transaction(0), batch_transaction(1)],
    )
    .unwrap();

    let view = projection.context_view(&session_id());
    let open = view.open_turns();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].turn_id(), &turn_id());
    assert_eq!(open[0].operation_id(), &operation_id());
    let batch = open[0].unfilled_batch().expect("batch lacks results");
    assert_eq!(batch.tool_batch_id(), &ToolBatchId::new("batch-1"));
    assert_eq!(
        batch.unfilled_call_ids(),
        &[ToolCallId::new("call-1"), ToolCallId::new("call-2")]
    );
}

#[test]
fn open_turns_partial_results_leave_the_missing_suffix() {
    let mut projection = SessionProjection::empty();
    apply_all(
        &mut projection,
        &[start_transaction(0), batch_transaction(1)],
    )
    .unwrap();
    // C_k is atomic in production; a lone-result transaction is still
    // structurally impossible, so complete the batch to observe the
    // resolved shape instead.
    apply_all(
        &mut projection,
        &[SessionTransaction::linear(
            session_id(),
            SessionRevision::new(2),
            vec![
                tool_result(SessionToolResult::success(ToolCallId::new("call-1"), "a")),
                tool_result(SessionToolResult::success(ToolCallId::new("call-2"), "b")),
            ],
        )],
    )
    .unwrap();

    let view = projection.context_view(&session_id());
    let open = view.open_turns();
    assert_eq!(open.len(), 1, "turn stays open without a terminal outcome");
    assert!(
        open[0].unfilled_batch().is_none(),
        "a fully resolved batch reports no unfilled calls"
    );
}

#[test]
fn open_turns_is_empty_for_cleanly_terminated_sessions() {
    let mut projection = SessionProjection::empty();
    apply_all(
        &mut projection,
        &[
            start_transaction(0),
            SessionTransaction::linear(
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
                        outcome: OperationOutcome::Succeeded,
                    },
                ],
            ),
        ],
    )
    .unwrap();
    assert!(
        projection
            .context_view(&session_id())
            .open_turns()
            .is_empty()
    );
}

#[test]
fn open_turns_lists_multiple_remnants_in_source_order() {
    let mut projection = SessionProjection::empty();
    // Two operations that both started turns and never terminated: the
    // projection does not forbid starting a new turn while one is open.
    apply_all(
        &mut projection,
        &[
            start_transaction(0),
            SessionTransaction::linear(
                session_id(),
                SessionRevision::new(1),
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
                        parts: SessionUserPart::text_parts("second"),
                    },
                ],
            ),
        ],
    )
    .unwrap();

    let view = projection.context_view(&session_id());
    let ids = view
        .open_turns()
        .iter()
        .map(|open| open.turn_id().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["turn-1", "turn-2"]);
}

#[test]
fn sealing_the_oldest_remnant_keeps_the_younger_one_open() {
    let mut projection = SessionProjection::empty();
    apply_all(
        &mut projection,
        &[
            start_transaction(0),
            SessionTransaction::linear(
                session_id(),
                SessionRevision::new(1),
                vec![
                    SessionEntryKind::OperationStarted {
                        operation_id: OperationId::new("op-2"),
                    },
                    SessionEntryKind::TurnStarted {
                        operation_id: OperationId::new("op-2"),
                        turn_id: TurnId::new("turn-2"),
                    },
                ],
            ),
            SessionTransaction::linear(
                session_id(),
                SessionRevision::new(2),
                terminal_entries(CancelReason::Abandoned),
            ),
        ],
    )
    .unwrap();

    let view = projection.context_view(&session_id());
    let ids = view
        .open_turns()
        .iter()
        .map(|open| open.turn_id().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["turn-2"], "sealed turn leaves, younger remnant stays");
}

// ---------------------------------------------------------------- 双路径一致

#[test]
fn apply_and_replay_agree_on_seal_transactions_and_open_turns() {
    let mut seal = vec![
        tool_result(SessionToolResult::interrupted(ToolCallId::new("call-1"))),
        tool_result(SessionToolResult::interrupted(ToolCallId::new("call-2"))),
    ];
    seal.extend(terminal_entries(CancelReason::Abandoned));
    let transactions = [
        start_transaction(0),
        batch_transaction(1),
        SessionTransaction::linear(session_id(), SessionRevision::new(2), seal),
    ];

    let mut applied = SessionProjection::empty();
    let mut committed = Vec::new();
    for transaction in &transactions {
        let result = applied.apply(transaction).expect("apply");
        committed.push(result.entries().to_vec());
        applied = result.into_projection();
    }

    // Mid-history agreement: replay only the first two transactions and the
    // remnant must be visible identically on both paths.
    let mut replayed = SessionProjection::empty();
    replayed.replay(&committed[0]).expect("replay start");
    replayed.replay(&committed[1]).expect("replay batch");
    let mut mid_applied = SessionProjection::empty();
    apply_all(&mut mid_applied, &transactions[..2]).unwrap();
    assert_eq!(
        replayed.context_view(&session_id()),
        mid_applied.context_view(&session_id())
    );

    replayed.replay(&committed[2]).expect("replay seal");
    assert_eq!(replayed.revision(), applied.revision());
    assert_eq!(replayed.current_leaf(), applied.current_leaf());
    assert_eq!(
        replayed.context_view(&session_id()),
        applied.context_view(&session_id())
    );
}

#[test]
fn memory_store_reports_open_turns_and_accepts_seals() {
    let store = MemorySessionStore::new();
    block_on(store.commit(start_transaction(0))).expect("start");
    block_on(store.commit(batch_transaction(1))).expect("batch");

    let view = block_on(store.context_view(&session_id())).expect("view");
    assert_eq!(view.open_turns().len(), 1);
    assert!(view.open_turns()[0].unfilled_batch().is_some());

    let mut seal = vec![
        tool_result(SessionToolResult::interrupted(ToolCallId::new("call-1"))),
        tool_result(SessionToolResult::interrupted(ToolCallId::new("call-2"))),
    ];
    seal.extend(terminal_entries(CancelReason::Abandoned));
    block_on(store.commit(SessionTransaction::linear(
        session_id(),
        SessionRevision::new(2),
        seal,
    )))
    .expect("seal commit");

    let view = block_on(store.context_view(&session_id())).expect("view");
    assert!(view.open_turns().is_empty());
}
