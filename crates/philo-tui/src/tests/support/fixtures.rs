//! Read-only durable session fixtures.

use philo_agent_service::{
    DurableSessionView, FrontendAssistantBlock, FrontendAvailability, FrontendContextMessage,
    FrontendEpoch, FrontendGeneration, FrontendRevision, FrontendSnapshot,
    FrontendToolResultOutcome, FrontendUserPart, LiveOperationSnapshot, ServiceHealth,
};

pub(crate) fn session_view(id: &str) -> DurableSessionView {
    DurableSessionView {
        session_id: id.to_owned(),
        title: None,
        revision: 3,
        messages: vec![
            FrontendContextMessage::User {
                parts: vec![FrontendUserPart::Text("count the files".to_owned())],
            },
            FrontendContextMessage::AssistantToolCalls {
                tool_batch_id: format!("{id}-batch"),
                blocks: vec![FrontendAssistantBlock::ToolCall {
                    id: format!("{id}-call"),
                    name: "read_file".to_owned(),
                    arguments: r#"{"path":"src/main.rs"}"#.to_owned(),
                }],
            },
            FrontendContextMessage::ToolResult {
                tool_call_id: format!("{id}-call"),
                outcome: FrontendToolResultOutcome::Success {
                    content: "fn main() {}".to_owned(),
                },
            },
            FrontendContextMessage::Assistant {
                blocks: vec![FrontendAssistantBlock::Text {
                    text: "one file".to_owned(),
                }],
            },
        ],
        open_turns: Vec::new(),
        settled_turn_boundaries: Vec::new(),
        latest_compaction_boundary: None,
    }
}

pub(crate) fn image_session_view(id: &str) -> DurableSessionView {
    DurableSessionView {
        session_id: id.to_owned(),
        title: None,
        revision: 1,
        messages: vec![FrontendContextMessage::User {
            parts: vec![
                FrontendUserPart::Text("look at this".to_owned()),
                FrontendUserPart::Image {
                    media_type: "image/png".to_owned(),
                    bytes: vec![1, 2, 3, 4],
                },
            ],
        }],
        open_turns: Vec::new(),
        settled_turn_boundaries: Vec::new(),
        latest_compaction_boundary: None,
    }
}

pub(crate) fn empty_session_view(id: &str) -> DurableSessionView {
    DurableSessionView {
        session_id: id.to_owned(),
        title: None,
        revision: 0,
        messages: Vec::new(),
        open_turns: Vec::new(),
        settled_turn_boundaries: Vec::new(),
        latest_compaction_boundary: None,
    }
}

fn test_generation() -> FrontendGeneration {
    FrontendGeneration {
        generation_id: "g-1".to_owned(),
        model_name: "m".to_owned(),
        reasoning_effort: None,
        tool_names: Vec::new(),
    }
}

pub(crate) fn idle_snapshot(session_id: &str) -> FrontendSnapshot {
    FrontendSnapshot {
        epoch: FrontendEpoch::INITIAL,
        revision: FrontendRevision::new(1),
        current_session_id: Some(session_id.to_owned()),
        durable_session_view: Some(empty_session_view(session_id)),
        live: LiveOperationSnapshot::default(),
        queued: Vec::new(),
        maintenance: None,
        availability: FrontendAvailability::Idle,
        generation: test_generation(),
        usage: None,
        pending_confirmations: Vec::new(),
        config_notices: Vec::new(),
        health: ServiceHealth::Ok,
    }
}

pub(crate) fn busy_snapshot(session_id: &str, operation_id: &str) -> FrontendSnapshot {
    let mut snapshot = idle_snapshot(session_id);
    snapshot.availability = FrontendAvailability::Busy {
        operation_id: operation_id.to_owned(),
    };
    snapshot.live.operation_id = Some(operation_id.to_owned());
    snapshot
}
