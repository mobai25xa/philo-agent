//! Session-history projection: durable frontend views become transcript
//! lines. Pure and read-only — the TUI never writes a session, and images
//! render as placeholder text (no inline terminal graphics).
//!
//! This is session replay, not the composer's input-history recall.

use philo_agent_service::{
    DurableSessionView, FrontendAssistantBlock, FrontendContextMessage, FrontendToolResultOutcome,
    FrontendUserPart,
};

use super::transcript::{LineKind, TranscriptLine, compact_args, line, preview, user_block};

/// Replays one session's model-visible context as transcript lines.
pub(crate) fn history_lines(view: &DurableSessionView) -> Vec<TranscriptLine> {
    view.messages.iter().flat_map(message_lines).collect()
}

/// Condensed preview for the session picker: the opening lines of the
/// session, truncated with a marker when more follows.
pub(crate) fn preview_lines(view: &DurableSessionView, max_lines: usize) -> Vec<String> {
    let lines = history_lines(view);
    if lines.is_empty() {
        return vec!["(empty session)".to_owned()];
    }
    let visible: Vec<&TranscriptLine> = lines.iter().filter(|line| !line.text.is_empty()).collect();
    if visible.is_empty() {
        return vec!["(empty session)".to_owned()];
    }
    let mut texts: Vec<String> = visible
        .iter()
        .take(max_lines)
        .map(|line| preview(&line.text, 120))
        .collect();
    if visible.len() > max_lines
        && let Some(last) = texts.last_mut()
    {
        *last = "...".to_owned();
    }
    texts
}

fn message_lines(message: &FrontendContextMessage) -> Vec<TranscriptLine> {
    match message {
        FrontendContextMessage::Summary { text } => text
            .split('\n')
            .map(|text| line(LineKind::Notice, format!("[summary] {text}")))
            .collect(),
        FrontendContextMessage::User { parts } => user_block(parts.iter().map(user_part_text)),
        FrontendContextMessage::Assistant { blocks } => assistant_block_lines(blocks, false),
        FrontendContextMessage::AssistantToolCalls { blocks, .. } => {
            assistant_block_lines(blocks, true)
        }
        FrontendContextMessage::ToolResult { outcome, .. } => {
            // Replay keeps the older `ok · {content}` summary: ToolDisplay
            // is not durable, so live cards cannot be reconstructed.
            vec![line(
                LineKind::Tool,
                format!("  └ {}", outcome_text(outcome)),
            )]
        }
    }
}

fn assistant_block_lines(
    blocks: &[FrontendAssistantBlock],
    include_tool_calls: bool,
) -> Vec<TranscriptLine> {
    let mut lines = Vec::new();
    for block in blocks {
        match block {
            FrontendAssistantBlock::Text { text } => {
                lines.push(line(LineKind::Answer, text.clone()));
            }
            FrontendAssistantBlock::ToolCall {
                name, arguments, ..
            } if include_tool_calls => lines.push(line(
                LineKind::Tool,
                format!("▸ {name}  {}", compact_args(arguments)),
            )),
            FrontendAssistantBlock::ToolCall { .. } => {}
        }
    }
    lines
}

fn user_part_text(part: &FrontendUserPart) -> String {
    match part {
        FrontendUserPart::Text(text) => preview(text, 200),
        FrontendUserPart::Image { media_type, bytes } => {
            format!("[image {media_type}, {} bytes]", bytes.len())
        }
    }
}

fn outcome_text(outcome: &FrontendToolResultOutcome) -> String {
    match outcome {
        FrontendToolResultOutcome::Success { content } => format!("ok · {}", preview(content, 80)),
        FrontendToolResultOutcome::Error { code, message } => {
            format!("error {code} · {}", preview(message, 80))
        }
        FrontendToolResultOutcome::Cancelled => "cancelled (never executed)".to_owned(),
        FrontendToolResultOutcome::Interrupted => {
            "interrupted (execution state unknown)".to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::support::{empty_session_view, image_session_view, session_view};

    #[test]
    fn history_renders_user_assistant_and_tool_facts() {
        let view = session_view("s-1");
        let rendered: Vec<String> = history_lines(&view)
            .iter()
            .map(|line| format!("{:?}: {}", line.kind, line.text))
            .collect();
        assert_eq!(
            rendered,
            [
                "User: ",
                "User: › count the files",
                "User: ",
                "Tool: ▸ read_file  path: src/main.rs",
                "Tool:   └ ok · fn main() {}",
                "Answer: one file",
            ]
        );
    }

    #[test]
    fn images_render_as_placeholder_text() {
        let view = image_session_view("s-img");
        let lines = history_lines(&view);
        let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(
            texts,
            ["", "› look at this", "  [image image/png, 4 bytes]", ""]
        );
    }

    #[test]
    fn tool_batch_renders_text_and_calls_in_block_order() {
        let lines = message_lines(&FrontendContextMessage::AssistantToolCalls {
            tool_batch_id: "batch".to_owned(),
            blocks: vec![
                FrontendAssistantBlock::Text {
                    text: "let me look\nthen call".to_owned(),
                },
                FrontendAssistantBlock::ToolCall {
                    id: "c".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: r#"{"path":"src/main.rs"}"#.to_owned(),
                },
                FrontendAssistantBlock::Text {
                    text: "after".to_owned(),
                },
            ],
        });
        assert_eq!(
            lines,
            [
                line(LineKind::Answer, "let me look\nthen call"),
                line(LineKind::Tool, "▸ read_file  path: src/main.rs"),
                line(LineKind::Answer, "after"),
            ]
        );
    }

    #[test]
    fn compacted_history_marks_the_summary_as_prior_context() {
        let lines = message_lines(&FrontendContextMessage::Summary {
            text: "earlier request\nearlier answer".to_owned(),
        });
        assert_eq!(
            lines,
            [
                line(LineKind::Notice, "[summary] earlier request"),
                line(LineKind::Notice, "[summary] earlier answer"),
            ]
        );
    }

    #[test]
    fn preview_truncates_with_a_marker() {
        let view = session_view("s-1");
        assert_eq!(
            preview_lines(&view, 2),
            ["› count the files".to_owned(), "...".to_owned()]
        );
        let empty = empty_session_view("s-empty");
        assert_eq!(preview_lines(&empty, 4), ["(empty session)".to_owned()]);
    }
}
