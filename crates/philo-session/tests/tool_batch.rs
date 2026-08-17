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
    let blocks = vec![
        SessionAssistantBlock::ToolCall(SessionToolCall::new(ToolCallId::new("a"), "one", "{}")),
        SessionAssistantBlock::ToolCall(SessionToolCall::new(ToolCallId::new("b"), "two", "{}")),
    ];
    block_on(store.commit(SessionTransaction::linear(
        session.clone(),
        SessionRevision::new(1),
        vec![SessionEntryKind::AssistantToolCallBatch {
            turn_id: turn.clone(),
            model_call_id: "m".to_owned(),
            tool_batch_id: ToolBatchId::new("batch"),
            blocks,
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

fn started_projection() -> (SessionProjection, SessionId, TurnId) {
    let session = SessionId::new("s");
    let turn = TurnId::new("t");
    let applied = SessionProjection::empty()
        .apply(&SessionTransaction::linear(
            session.clone(),
            SessionRevision::ZERO,
            vec![
                SessionEntryKind::OperationStarted {
                    operation_id: OperationId::new("o"),
                },
                SessionEntryKind::TurnStarted {
                    operation_id: OperationId::new("o"),
                    turn_id: turn.clone(),
                },
                SessionEntryKind::UserMessage {
                    turn_id: turn.clone(),
                    parts: SessionUserPart::text_parts("hi"),
                },
            ],
        ))
        .expect("turn start is valid");
    (applied.into_projection(), session, turn)
}

#[test]
fn mixed_text_and_tool_call_blocks_commit_and_project_in_source_order() {
    let store = MemorySessionStore::new();
    let session = SessionId::new("s");
    let turn = TurnId::new("t");
    block_on(store.commit(SessionTransaction::linear(
        session.clone(),
        SessionRevision::ZERO,
        vec![
            SessionEntryKind::OperationStarted {
                operation_id: OperationId::new("o"),
            },
            SessionEntryKind::TurnStarted {
                operation_id: OperationId::new("o"),
                turn_id: turn.clone(),
            },
            SessionEntryKind::UserMessage {
                turn_id: turn.clone(),
                parts: SessionUserPart::text_parts("hi"),
            },
        ],
    )))
    .unwrap();

    let blocks = vec![
        SessionAssistantBlock::Text {
            text: "preamble".into(),
        },
        SessionAssistantBlock::ToolCall(SessionToolCall::new(ToolCallId::new("a"), "one", "{}")),
    ];
    block_on(store.commit(SessionTransaction::linear(
        session.clone(),
        SessionRevision::new(1),
        vec![SessionEntryKind::AssistantToolCallBatch {
            turn_id: turn,
            model_call_id: "m".to_owned(),
            tool_batch_id: ToolBatchId::new("batch"),
            blocks: blocks.clone(),
        }],
    )))
    .expect("mixed Text then ToolCall is a valid batch");

    let view = block_on(store.context_view(&session)).unwrap();
    assert_eq!(
        &view.messages()[1],
        &ContextMessage::AssistantToolCalls {
            tool_batch_id: ToolBatchId::new("batch"),
            blocks,
        }
    );
}

#[test]
fn empty_text_block_is_rejected() {
    let (projection, session, turn) = started_projection();
    let batch_error = projection
        .apply(&SessionTransaction::linear(
            session.clone(),
            SessionRevision::new(1),
            vec![SessionEntryKind::AssistantToolCallBatch {
                turn_id: turn.clone(),
                model_call_id: "m".to_owned(),
                tool_batch_id: ToolBatchId::new("batch"),
                blocks: vec![
                    SessionAssistantBlock::Text {
                        text: String::new(),
                    },
                    SessionAssistantBlock::ToolCall(SessionToolCall::new(
                        ToolCallId::new("a"),
                        "one",
                        "{}",
                    )),
                ],
            }],
        ))
        .expect_err("empty Text in a batch must be rejected");
    assert_eq!(
        batch_error,
        SessionError::Validation(SessionValidationError::InvalidToolBatch {
            turn_id: turn.clone()
        })
    );

    let message_error = projection
        .apply(&SessionTransaction::linear(
            session,
            SessionRevision::new(1),
            vec![SessionEntryKind::AssistantMessage {
                turn_id: turn.clone(),
                blocks: vec![SessionAssistantBlock::Text {
                    text: String::new(),
                }],
            }],
        ))
        .expect_err("empty Text in a final message must be rejected");
    assert_eq!(
        message_error,
        SessionError::Validation(SessionValidationError::InvalidTurnReference { turn_id: turn })
    );
}

#[test]
fn tool_call_in_assistant_message_is_rejected() {
    let (projection, session, turn) = started_projection();
    let error = projection
        .apply(&SessionTransaction::linear(
            session,
            SessionRevision::new(1),
            vec![SessionEntryKind::AssistantMessage {
                turn_id: turn.clone(),
                blocks: vec![SessionAssistantBlock::ToolCall(SessionToolCall::new(
                    ToolCallId::new("a"),
                    "one",
                    "{}",
                ))],
            }],
        ))
        .expect_err("a final message must not contain ToolCall");
    assert_eq!(
        error,
        SessionError::Validation(SessionValidationError::InvalidTurnReference { turn_id: turn })
    );
}

#[test]
fn tool_batch_without_tool_call_is_rejected() {
    let (projection, session, turn) = started_projection();
    for blocks in [
        Vec::new(),
        vec![SessionAssistantBlock::Text {
            text: "preamble".into(),
        }],
    ] {
        let error = projection
            .apply(&SessionTransaction::linear(
                session.clone(),
                SessionRevision::new(1),
                vec![SessionEntryKind::AssistantToolCallBatch {
                    turn_id: turn.clone(),
                    model_call_id: "m".to_owned(),
                    tool_batch_id: ToolBatchId::new("batch"),
                    blocks,
                }],
            ))
            .expect_err("a batch must contain at least one ToolCall");
        assert_eq!(
            error,
            SessionError::Validation(SessionValidationError::InvalidToolBatch {
                turn_id: turn.clone()
            })
        );
    }
}
