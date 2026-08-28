//! SESSION-004: shared validation core (apply / replay / context_view).

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use philo_session::{
    EntryId, MemorySessionStore, OperationId, OperationOutcome, SessionAssistantBlock,
    SessionEntry, SessionEntryKind, SessionError, SessionId, SessionProjection, SessionRevision,
    SessionStore, SessionTokenUsage, SessionToolCall, SessionToolResult, SessionTransaction,
    SessionUserPart, SessionValidationError, ToolBatchId, ToolCallId, TurnId, TurnOutcome,
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
    SessionId::new("proj-session")
}

fn start_transaction(revision: SessionRevision) -> SessionTransaction {
    let operation = OperationId::new("op-1");
    let turn = TurnId::new("turn-1");
    SessionTransaction::linear(
        session_id(),
        revision,
        vec![
            SessionEntryKind::OperationStarted {
                operation_id: operation.clone(),
            },
            SessionEntryKind::TurnStarted {
                operation_id: operation,
                turn_id: turn.clone(),
            },
            SessionEntryKind::UserMessage {
                turn_id: turn,
                parts: SessionUserPart::text_parts("hello"),
            },
        ],
    )
}

fn batch_transaction(revision: SessionRevision) -> SessionTransaction {
    SessionTransaction::linear(
        session_id(),
        revision,
        vec![SessionEntryKind::AssistantToolCallBatch {
            turn_id: TurnId::new("turn-1"),
            model_call_id: "model-call-1".to_owned(),
            tool_batch_id: ToolBatchId::new("batch-1"),
            blocks: vec![SessionAssistantBlock::ToolCall(SessionToolCall::new(
                ToolCallId::new("call-1"),
                "read",
                r#"{"path":"a.txt"}"#,
            ))],
        }],
    )
}

fn results_transaction(revision: SessionRevision) -> SessionTransaction {
    SessionTransaction::linear(
        session_id(),
        revision,
        vec![SessionEntryKind::ToolResult {
            turn_id: TurnId::new("turn-1"),
            tool_batch_id: ToolBatchId::new("batch-1"),
            result: SessionToolResult::success(ToolCallId::new("call-1"), "content"),
        }],
    )
}

fn settle_transaction(revision: SessionRevision) -> SessionTransaction {
    SessionTransaction::linear(
        session_id(),
        revision,
        vec![
            SessionEntryKind::AssistantMessage {
                turn_id: TurnId::new("turn-1"),
                blocks: vec![SessionAssistantBlock::Text {
                    text: "done".into(),
                }],
            },
            SessionEntryKind::TurnTerminated {
                turn_id: TurnId::new("turn-1"),
                outcome: TurnOutcome::Succeeded,
            },
            SessionEntryKind::OperationSettled {
                operation_id: OperationId::new("op-1"),
                outcome: OperationOutcome::Succeeded,
                usage: None,
            },
        ],
    )
}

/// Applies the full four-save-point turn, returning each transaction's entries.
fn apply_full_turn(projection: &mut SessionProjection) -> Vec<Vec<SessionEntry>> {
    let mut committed = Vec::new();
    for transaction in [
        start_transaction(SessionRevision::new(0)),
        batch_transaction(SessionRevision::new(1)),
        results_transaction(SessionRevision::new(2)),
        settle_transaction(SessionRevision::new(3)),
    ] {
        assert_eq!(transaction.expected_revision(), projection.revision());
        let applied = projection.apply(&transaction).expect("valid transaction");
        committed.push(applied.entries().to_vec());
        *projection = applied.into_projection();
    }
    committed
}

#[test]
fn apply_allocates_deterministic_ids_and_advances_revision() {
    let mut projection = SessionProjection::empty();
    let committed = apply_full_turn(&mut projection);
    assert_eq!(projection.revision(), SessionRevision::new(4));

    let ids: Vec<&str> = committed
        .iter()
        .flatten()
        .map(|entry| entry.id().as_str())
        .collect();
    let expected: Vec<String> = (1..=8)
        .map(|index| format!("proj-session:entry:{index}"))
        .collect();
    assert_eq!(ids, expected.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(
        projection.current_leaf().map(EntryId::as_str),
        Some("proj-session:entry:8")
    );

    // Parents form one linear chain across transactions.
    let all: Vec<&SessionEntry> = committed.iter().flatten().collect();
    assert_eq!(all[0].parent(), None);
    for pair in all.windows(2) {
        assert_eq!(pair[1].parent(), Some(pair[0].id()));
    }
}

#[test]
fn replay_rebuilds_the_same_projection_as_apply() {
    let mut applied_projection = SessionProjection::empty();
    let committed = apply_full_turn(&mut applied_projection);

    let mut replayed = SessionProjection::empty();
    for transaction_entries in &committed {
        replayed
            .replay(transaction_entries)
            .expect("committed facts replay cleanly");
    }

    assert_eq!(replayed.revision(), applied_projection.revision());
    assert_eq!(replayed.current_leaf(), applied_projection.current_leaf());
    assert_eq!(
        replayed.context_view(&session_id()),
        applied_projection.context_view(&session_id()),
        "apply and replay must project identical context"
    );
}

#[test]
fn replay_then_apply_continues_ids_and_revisions_seamlessly() {
    let mut source = SessionProjection::empty();
    let committed = apply_full_turn(&mut source);

    // Rebuild only the first three transactions, then apply the fourth live.
    let mut projection = SessionProjection::empty();
    for transaction_entries in &committed[..3] {
        projection.replay(transaction_entries).expect("replay");
    }
    let applied = projection
        .apply(&settle_transaction(SessionRevision::new(3)))
        .expect("apply continues after replay");
    assert_eq!(
        applied.entries()[0].id().as_str(),
        "proj-session:entry:6",
        "entry ids continue from the replayed count"
    );
    assert_eq!(applied.projection().revision(), SessionRevision::new(4));
    assert_eq!(
        applied.projection().context_view(&session_id()),
        source.context_view(&session_id())
    );
}

#[test]
fn rebuilt_entries_pass_replay_only_when_the_chain_is_intact() {
    let mut source = SessionProjection::empty();
    let committed = apply_full_turn(&mut source);

    // Rebuild from persisted fields: same facts replay cleanly.
    let rebuilt: Vec<SessionEntry> = committed[0]
        .iter()
        .map(|entry| {
            SessionEntry::from_persisted(
                entry.id().clone(),
                entry.parent().cloned(),
                entry.kind().clone(),
            )
        })
        .collect();
    let mut projection = SessionProjection::empty();
    projection.replay(&rebuilt).expect("intact chain replays");

    // A broken parent chain is rejected.
    let mut broken = rebuilt.clone();
    broken[1] = SessionEntry::from_persisted(
        broken[1].id().clone(),
        Some(EntryId::new("forged:parent")),
        broken[1].kind().clone(),
    );
    let error = SessionProjection::empty().replay(&broken).err().unwrap();
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidParent { entry_index: 1 })
    );
}

#[test]
fn replay_rejects_reordered_transactions() {
    let mut source = SessionProjection::empty();
    let committed = apply_full_turn(&mut source);

    // Skipping the batch transaction: the tool-result purity check runs
    // first (same order as apply) and rejects results without their batch.
    let mut projection = SessionProjection::empty();
    projection.replay(&committed[0]).expect("start replays");
    let purity_error = projection.replay(&committed[2]).err().unwrap();
    assert!(matches!(
        purity_error,
        SessionError::Validation(SessionValidationError::InvalidToolResult { .. })
    ));

    // Skipping a transaction without tool results breaks the parent chain.
    let mut chain = SessionProjection::empty();
    chain.replay(&committed[0]).expect("start replays");
    let chain_error = chain.replay(&committed[3]).err().unwrap();
    assert_eq!(
        chain_error,
        SessionError::Validation(SessionValidationError::InvalidParent { entry_index: 0 })
    );

    // Even with a repaired chain, results before their batch are invalid.
    let forged: Vec<SessionEntry> = committed[2]
        .iter()
        .map(|entry| {
            SessionEntry::from_persisted(
                EntryId::new("forged:1"),
                Some(EntryId::new("proj-session:entry:3")),
                entry.kind().clone(),
            )
        })
        .collect();
    let mut fresh = SessionProjection::empty();
    fresh.replay(&committed[0]).expect("start replays");
    let semantic_error = fresh.replay(&forged).err().unwrap();
    assert!(matches!(
        semantic_error,
        SessionError::Validation(SessionValidationError::InvalidToolResult { .. })
    ));
}

#[test]
fn replay_rejects_empty_transactions() {
    let error = SessionProjection::empty().replay(&[]).err().unwrap();
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::EmptyTransaction)
    );
}

#[test]
fn apply_does_not_mutate_the_source_projection() {
    let projection = SessionProjection::empty();
    let applied = projection
        .apply(&start_transaction(SessionRevision::new(0)))
        .expect("valid transaction");
    assert_eq!(projection.revision(), SessionRevision::new(0));
    assert_eq!(projection.current_leaf(), None);
    assert_eq!(applied.projection().revision(), SessionRevision::new(1));
}

#[test]
fn memory_store_matches_the_projection_core_exactly() {
    let mut projection = SessionProjection::empty();
    let committed = apply_full_turn(&mut projection);

    let store = MemorySessionStore::new();
    let mut revision = SessionRevision::ZERO;
    let mut store_entries = Vec::new();
    for transaction in [
        start_transaction(SessionRevision::new(0)),
        batch_transaction(SessionRevision::new(1)),
        results_transaction(SessionRevision::new(2)),
        settle_transaction(SessionRevision::new(3)),
    ] {
        let commit = block_on(store.commit(transaction)).expect("store commit");
        revision = commit.revision();
        store_entries.push(commit.entries().to_vec());
    }
    assert_eq!(revision, projection.revision());
    assert_eq!(store_entries, committed, "same ids, parents, and facts");
    assert_eq!(
        block_on(store.context_view(&session_id())).expect("view"),
        projection.context_view(&session_id())
    );
}

#[test]
fn store_unavailable_carries_diagnostic_text() {
    let error = SessionError::store_unavailable("disk on fire");
    let SessionError::StoreUnavailable { reason } = &error else {
        panic!("expected StoreUnavailable");
    };
    assert_eq!(reason, "disk on fire");
}

#[test]
fn store_busy_carries_diagnostic_text() {
    let error = SessionError::store_busy("queue full");
    let SessionError::StoreBusy { reason } = &error else {
        panic!("expected StoreBusy");
    };
    assert_eq!(reason, "queue full");
}

#[test]
fn context_view_exposes_latest_settled_usage() {
    let mut projection = SessionProjection::empty();
    let usage = SessionTokenUsage {
        input_tokens: Some(100),
        output_tokens: Some(50),
        ..Default::default()
    };
    let settle = SessionTransaction::linear(
        session_id(),
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
                parts: SessionUserPart::text_parts("hi"),
            },
            SessionEntryKind::AssistantMessage {
                turn_id: TurnId::new("turn-1"),
                blocks: vec![SessionAssistantBlock::Text {
                    text: "hello".into(),
                }],
            },
            SessionEntryKind::TurnTerminated {
                turn_id: TurnId::new("turn-1"),
                outcome: TurnOutcome::Succeeded,
            },
            SessionEntryKind::OperationSettled {
                operation_id: OperationId::new("op-1"),
                outcome: OperationOutcome::Succeeded,
                usage: Some(usage),
            },
        ],
    );
    let applied = projection.apply(&settle).expect("valid transaction");
    projection = applied.into_projection();
    let view = projection.context_view(&session_id());
    assert_eq!(view.latest_usage(), Some(usage));
}

#[test]
fn context_view_usage_none_before_first_settled_turn() {
    let projection = SessionProjection::empty();
    let view = projection.context_view(&session_id());
    assert_eq!(view.latest_usage(), None);
}
