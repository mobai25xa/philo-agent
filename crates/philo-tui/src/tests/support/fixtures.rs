//! Read-only durable session fixtures.

use philo_agent_service::{
    DurableSessionView, FrontendAssistantBlock, FrontendContextMessage, FrontendToolResultOutcome,
    FrontendUserPart,
};

pub(crate) fn session_view(id: &str) -> DurableSessionView {
    DurableSessionView {
        session_id: id.to_owned(),
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
        revision: 0,
        messages: Vec::new(),
        open_turns: Vec::new(),
        settled_turn_boundaries: Vec::new(),
        latest_compaction_boundary: None,
    }
}
