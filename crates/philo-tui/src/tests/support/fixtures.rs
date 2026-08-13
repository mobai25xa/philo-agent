//! Read-only tool and session fixtures.

use philo_agent_runtime::{EffectClass, ToolDefinition};
use philo_session::{
    SessionContextView, SessionEntryKind, SessionProjection, SessionRevision, SessionToolCall,
    SessionToolResult, SessionTransaction, SessionUserPart, ToolBatchId, ToolCallId, TurnId,
};

pub(crate) fn tool(name: &str, class: EffectClass) -> ToolDefinition {
    ToolDefinition::simple(name, format!("{name} description"), class)
}

fn view_from(id: &str, kinds: Vec<SessionEntryKind>) -> SessionContextView {
    let session_id = philo_session::SessionId::new(id);
    SessionProjection::empty()
        .apply(&SessionTransaction::linear(
            session_id.clone(),
            SessionRevision::ZERO,
            kinds,
        ))
        .expect("fixture entries are a valid linear transaction")
        .into_projection()
        .context_view(&session_id)
}

pub(crate) fn session_view(id: &str) -> SessionContextView {
    let session_id = philo_session::SessionId::new(id);
    let operation_id = philo_session::OperationId::new(format!("{id}-op"));
    let turn_id = TurnId::new(format!("{id}-turn"));
    let tool_batch_id = ToolBatchId::new(format!("{id}-batch"));
    let call_id = ToolCallId::new(format!("{id}-call"));
    let transactions = [
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
                parts: SessionUserPart::text_parts("count the files"),
            },
        ],
        vec![SessionEntryKind::AssistantToolCallBatch {
            turn_id: turn_id.clone(),
            model_call_id: format!("{id}-model-call"),
            tool_batch_id: tool_batch_id.clone(),
            calls: vec![SessionToolCall::new(
                call_id.clone(),
                "read_file",
                r#"{"path":"src/main.rs"}"#,
            )],
        }],
        vec![SessionEntryKind::ToolResult {
            turn_id: turn_id.clone(),
            tool_batch_id,
            result: SessionToolResult::success(call_id, "fn main() {}"),
        }],
        vec![
            SessionEntryKind::AssistantMessage {
                turn_id: turn_id.clone(),
                content: "one file".to_owned(),
            },
            SessionEntryKind::TurnTerminated {
                turn_id,
                outcome: philo_session::TurnOutcome::Succeeded,
            },
            SessionEntryKind::OperationSettled {
                operation_id,
                outcome: philo_session::OperationOutcome::Succeeded,
            },
        ],
    ];
    let projection = transactions.into_iter().enumerate().fold(
        SessionProjection::empty(),
        |projection, (revision, kinds)| {
            projection
                .apply(&SessionTransaction::linear(
                    session_id.clone(),
                    SessionRevision::new(revision as u64),
                    kinds,
                ))
                .expect("fixture transactions form a valid completed turn")
                .into_projection()
        },
    );
    projection.context_view(&session_id)
}

pub(crate) fn image_session_view(id: &str) -> SessionContextView {
    let operation_id = philo_session::OperationId::new(format!("{id}-op"));
    let turn_id = TurnId::new(format!("{id}-turn"));
    view_from(
        id,
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
                parts: vec![
                    SessionUserPart::Text("look at this".to_owned()),
                    SessionUserPart::Image {
                        media_type: "image/png".to_owned(),
                        bytes: vec![1, 2, 3, 4],
                    },
                ],
            },
        ],
    )
}

pub(crate) fn empty_session_view(id: &str) -> SessionContextView {
    SessionProjection::empty().context_view(&philo_session::SessionId::new(id))
}
