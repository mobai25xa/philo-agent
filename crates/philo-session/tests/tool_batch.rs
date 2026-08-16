//! SESSION-002: 工具结果源序投影.

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

#[test]
fn tool_result_batch_projects_in_source_order() {
    let store = MemorySessionStore::new();
    let session = SessionId::new("s");
    let operation = OperationId::new("o");
    let turn = TurnId::new("t");
    block_on(store.commit(SessionTransaction::linear(
        session.clone(),
        SessionRevision::ZERO,
        vec![
            SessionEntryKind::OperationStarted {
                operation_id: operation.clone(),
            },
            SessionEntryKind::TurnStarted {
                operation_id: operation.clone(),
                turn_id: turn.clone(),
            },
            SessionEntryKind::UserMessage {
                turn_id: turn.clone(),
                parts: SessionUserPart::text_parts("hi"),
            },
        ],
    )))
    .unwrap();
    let calls = vec![
        SessionToolCall::new(ToolCallId::new("a"), "one", "{}"),
        SessionToolCall::new(ToolCallId::new("b"), "two", "{}"),
    ];
    block_on(store.commit(SessionTransaction::linear(
        session.clone(),
        SessionRevision::new(1),
        vec![SessionEntryKind::AssistantToolCallBatch {
            turn_id: turn.clone(),
            model_call_id: "m".to_owned(),
            tool_batch_id: ToolBatchId::new("batch"),
            calls,
        }],
    )))
    .unwrap();
    block_on(store.commit(SessionTransaction::linear(
        session.clone(),
        SessionRevision::new(2),
        vec![
            SessionEntryKind::ToolResult {
                turn_id: turn.clone(),
                tool_batch_id: ToolBatchId::new("batch"),
                result: SessionToolResult::success(ToolCallId::new("a"), "1"),
            },
            SessionEntryKind::ToolResult {
                turn_id: turn.clone(),
                tool_batch_id: ToolBatchId::new("batch"),
                result: SessionToolResult::error(ToolCallId::new("b"), "bad", "no"),
            },
        ],
    )))
    .unwrap();
    let view = block_on(store.context_view(&session)).unwrap();
    assert!(matches!(
        view.messages()[1],
        ContextMessage::AssistantToolCalls { .. }
    ));
    assert!(
        matches!(&view.messages()[2], ContextMessage::ToolResult { tool_call_id, .. } if tool_call_id.as_str() == "a")
    );
    assert!(
        matches!(&view.messages()[3], ContextMessage::ToolResult { tool_call_id, .. } if tool_call_id.as_str() == "b")
    );
}
