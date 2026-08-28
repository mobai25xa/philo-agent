//! SESSION-003: 多 batch 交错与拒绝矩阵.

use philo_session::*;
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

fn started_store(session: &SessionId, turn: &TurnId) -> MemorySessionStore {
    let store = MemorySessionStore::new();
    let operation = OperationId::new("o");
    block_on(store.commit(SessionTransaction::linear(
        session.clone(),
        SessionRevision::ZERO,
        vec![
            SessionEntryKind::OperationStarted {
                operation_id: operation.clone(),
            },
            SessionEntryKind::TurnStarted {
                operation_id: operation,
                turn_id: turn.clone(),
            },
            SessionEntryKind::UserMessage {
                turn_id: turn.clone(),
                parts: SessionUserPart::text_parts("hi"),
            },
        ],
    )))
    .unwrap();
    store
}

fn batch_entry(turn: &TurnId, batch: &str, call_ids: &[&str]) -> SessionEntryKind {
    SessionEntryKind::AssistantToolCallBatch {
        turn_id: turn.clone(),
        model_call_id: format!("model-for-{batch}"),
        tool_batch_id: ToolBatchId::new(batch),
        blocks: call_ids
            .iter()
            .map(|id| {
                SessionAssistantBlock::ToolCall(SessionToolCall::new(
                    ToolCallId::new(*id),
                    "tool",
                    "{}",
                ))
            })
            .collect(),
    }
}

fn result_entry(turn: &TurnId, batch: &str, call_id: &str) -> SessionEntryKind {
    SessionEntryKind::ToolResult {
        turn_id: turn.clone(),
        tool_batch_id: ToolBatchId::new(batch),
        result: SessionToolResult::success(ToolCallId::new(call_id), "ok"),
    }
}

fn commit(
    store: &MemorySessionStore,
    session: &SessionId,
    revision: u64,
    kinds: Vec<SessionEntryKind>,
) -> Result<SessionCommit, SessionError> {
    block_on(store.commit(SessionTransaction::linear(
        session.clone(),
        SessionRevision::new(revision),
        kinds,
    )))
}

#[test]
fn two_rounds_project_interleaved_in_source_order() {
    let session = SessionId::new("s");
    let turn = TurnId::new("t");
    let store = started_store(&session, &turn);

    commit(&store, &session, 1, vec![batch_entry(&turn, "b1", &["a"])]).unwrap();
    commit(&store, &session, 2, vec![result_entry(&turn, "b1", "a")]).unwrap();
    commit(
        &store,
        &session,
        3,
        vec![batch_entry(&turn, "b2", &["c", "d"])],
    )
    .unwrap();
    commit(
        &store,
        &session,
        4,
        vec![
            result_entry(&turn, "b2", "c"),
            result_entry(&turn, "b2", "d"),
        ],
    )
    .unwrap();
    commit(
        &store,
        &session,
        5,
        vec![
            SessionEntryKind::AssistantMessage {
                turn_id: turn.clone(),
                blocks: vec![SessionAssistantBlock::Text {
                    text: "done".into(),
                }],
            },
            SessionEntryKind::TurnTerminated {
                turn_id: turn.clone(),
                outcome: TurnOutcome::Succeeded,
            },
            SessionEntryKind::OperationSettled {
                operation_id: OperationId::new("o"),
                outcome: OperationOutcome::Succeeded,
                usage: None,
                generation: None,
            },
        ],
    )
    .unwrap();

    let view = block_on(store.context_view(&session)).unwrap();
    let kinds: Vec<&str> = view
        .messages()
        .iter()
        .map(|message| match message {
            ContextMessage::Summary { .. } => "summary",
            ContextMessage::User { .. } => "user",
            ContextMessage::AssistantToolCalls { .. } => "calls",
            ContextMessage::ToolResult { .. } => "result",
            ContextMessage::Assistant { .. } => "assistant",
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "user",
            "calls",
            "result",
            "calls",
            "result",
            "result",
            "assistant"
        ]
    );
}

#[test]
fn new_batch_before_results_complete_is_rejected() {
    let session = SessionId::new("s");
    let turn = TurnId::new("t");
    let store = started_store(&session, &turn);

    commit(&store, &session, 1, vec![batch_entry(&turn, "b1", &["a"])]).unwrap();
    let error = commit(&store, &session, 2, vec![batch_entry(&turn, "b2", &["c"])])
        .expect_err("batch b1 has no results yet");
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidToolBatch {
            turn_id: turn.clone()
        })
    );

    let view = block_on(store.context_view(&session)).unwrap();
    assert_eq!(view.revision(), SessionRevision::new(2));
    assert_eq!(view.messages().len(), 2, "no partial write");
}

#[test]
fn result_referencing_previous_batch_is_rejected() {
    let session = SessionId::new("s");
    let turn = TurnId::new("t");
    let store = started_store(&session, &turn);

    commit(&store, &session, 1, vec![batch_entry(&turn, "b1", &["a"])]).unwrap();
    commit(&store, &session, 2, vec![result_entry(&turn, "b1", "a")]).unwrap();
    commit(&store, &session, 3, vec![batch_entry(&turn, "b2", &["c"])]).unwrap();

    let error = commit(&store, &session, 4, vec![result_entry(&turn, "b1", "a")])
        .expect_err("b1 is not the newest batch");
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidToolResult {
            turn_id: turn.clone()
        })
    );
}

#[test]
fn duplicate_batch_id_is_rejected() {
    let session = SessionId::new("s");
    let turn = TurnId::new("t");
    let store = started_store(&session, &turn);

    commit(&store, &session, 1, vec![batch_entry(&turn, "b1", &["a"])]).unwrap();
    commit(&store, &session, 2, vec![result_entry(&turn, "b1", "a")]).unwrap();

    let error = commit(&store, &session, 3, vec![batch_entry(&turn, "b1", &["c"])])
        .expect_err("batch ids must be unique within a turn");
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidToolBatch {
            turn_id: turn.clone()
        })
    );
}

#[test]
fn assistant_message_before_latest_results_is_rejected() {
    let session = SessionId::new("s");
    let turn = TurnId::new("t");
    let store = started_store(&session, &turn);

    commit(&store, &session, 1, vec![batch_entry(&turn, "b1", &["a"])]).unwrap();
    commit(&store, &session, 2, vec![result_entry(&turn, "b1", "a")]).unwrap();
    commit(&store, &session, 3, vec![batch_entry(&turn, "b2", &["c"])]).unwrap();

    let error = commit(
        &store,
        &session,
        4,
        vec![SessionEntryKind::AssistantMessage {
            turn_id: turn.clone(),
            blocks: vec![SessionAssistantBlock::Text {
                text: "too early".into(),
            }],
        }],
    )
    .expect_err("batch b2 is incomplete");
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidTurnOutcome {
            turn_id: turn.clone()
        })
    );
}

#[test]
fn multi_round_revision_conflict_has_no_partial_write() {
    let session = SessionId::new("s");
    let turn = TurnId::new("t");
    let store = started_store(&session, &turn);

    commit(&store, &session, 1, vec![batch_entry(&turn, "b1", &["a"])]).unwrap();
    commit(&store, &session, 2, vec![result_entry(&turn, "b1", "a")]).unwrap();

    let error = commit(&store, &session, 2, vec![batch_entry(&turn, "b2", &["c"])])
        .expect_err("stale revision");
    assert_eq!(
        error,
        SessionError::RevisionConflict {
            expected: SessionRevision::new(2),
            actual: SessionRevision::new(3),
        }
    );

    let view = block_on(store.context_view(&session)).unwrap();
    assert_eq!(view.revision(), SessionRevision::new(3));
    assert_eq!(view.messages().len(), 3, "user + calls + result only");

    commit(&store, &session, 3, vec![batch_entry(&turn, "b2", &["c"])])
        .expect("retry with fresh revision succeeds");
}
