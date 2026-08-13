//! Session-history projection: durable context messages become transcript
//! lines. Pure and read-only — the TUI never writes a session, and images
//! render as placeholder text (no inline terminal graphics).

use philo_session::{ContextMessage, SessionContextView, SessionUserPart, ToolResultOutcome};

use super::transcript::{LineKind, TranscriptLine, preview};

fn line(kind: LineKind, text: impl Into<String>) -> TranscriptLine {
    TranscriptLine {
        kind,
        text: text.into(),
    }
}

/// Replays one session's model-visible context as transcript lines.
pub(crate) fn history_lines(view: &SessionContextView) -> Vec<TranscriptLine> {
    view.messages().iter().flat_map(message_lines).collect()
}

/// Condensed preview for the session picker: the opening lines of the
/// session, truncated with a marker when more follows.
pub(crate) fn preview_lines(view: &SessionContextView, max_lines: usize) -> Vec<String> {
    let lines = history_lines(view);
    if lines.is_empty() {
        return vec!["(empty session)".to_owned()];
    }
    let mut texts: Vec<String> = lines
        .iter()
        .take(max_lines)
        .map(|line| preview(&line.text, 120))
        .collect();
    if lines.len() > max_lines
        && let Some(last) = texts.last_mut()
    {
        *last = "...".to_owned();
    }
    texts
}

fn message_lines(message: &ContextMessage) -> Vec<TranscriptLine> {
    match message {
        ContextMessage::User { parts } => parts.iter().map(user_part_line).collect(),
        ContextMessage::Assistant { content } => content
            .split('\n')
            .map(|text| line(LineKind::Answer, text.to_owned()))
            .collect(),
        ContextMessage::AssistantToolCalls { calls, .. } => calls
            .iter()
            .map(|call| {
                line(
                    LineKind::Tool,
                    format!("tool {}({})", call.name(), preview(call.arguments(), 60)),
                )
            })
            .collect(),
        ContextMessage::ToolResult { outcome, .. } => {
            vec![line(
                LineKind::Tool,
                format!("  -> {}", outcome_text(outcome)),
            )]
        }
    }
}

fn user_part_line(part: &SessionUserPart) -> TranscriptLine {
    match part {
        SessionUserPart::Text(text) => line(LineKind::User, format!("> {}", preview(text, 200))),
        SessionUserPart::Image { media_type, bytes } => line(
            LineKind::User,
            format!("> [image {media_type}, {} bytes]", bytes.len()),
        ),
    }
}

fn outcome_text(outcome: &ToolResultOutcome) -> String {
    match outcome {
        ToolResultOutcome::Success { content } => format!("ok: {}", preview(content, 80)),
        ToolResultOutcome::Error { code, message } => {
            format!("error {code}: {}", preview(message, 80))
        }
        ToolResultOutcome::Cancelled => "cancelled (never executed)".to_owned(),
        ToolResultOutcome::Interrupted => "interrupted (execution state unknown)".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::support::session_view;

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
                "User: > count the files",
                "Tool: tool read_file({\"path\":\"src/main.rs\"})",
                "Tool:   -> ok: fn main() {}",
                "Answer: one file",
            ]
        );
    }

    #[test]
    fn images_render_as_placeholder_text() {
        let view = crate::tests::support::image_session_view("s-img");
        let lines = history_lines(&view);
        let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(texts, ["> look at this", "> [image image/png, 4 bytes]"]);
    }

    #[test]
    fn preview_truncates_with_a_marker() {
        let view = session_view("s-1");
        assert_eq!(
            preview_lines(&view, 2),
            ["> count the files".to_owned(), "...".to_owned()]
        );
        let empty = crate::tests::support::empty_session_view("s-empty");
        assert_eq!(preview_lines(&empty, 4), ["(empty session)".to_owned()]);
    }
}
