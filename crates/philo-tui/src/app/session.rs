//! Session-history projection: durable frontend views become transcript
//! lines. Pure and read-only — the TUI never writes a session, and images
//! render as placeholder text (no inline terminal graphics).
//!
//! This is session replay, not the composer's input-history recall.
//!
//! Replay derives a display-channel projection from the durable model
//! channel facts the session store already keeps (`derive_display_for_replay`)
//! so replayed tool calls render through the same `default_card` path live
//! cards use. Fields the persisted facts cannot recover (file totals, elapsed
//! time, edit byte sizes) simply stay absent; the TUI card renderer leaves
//! their slots empty rather than guessing.

use std::collections::HashMap;

use philo_agent_service::{
    DurableSessionView, FrontendAssistantBlock, FrontendContextMessage, FrontendToolResult,
    FrontendToolResultOutcome, FrontendUserPart, derive_display_for_replay,
};

use super::tool_card;
use super::transcript::{
    CardBody, CardHeader, HeaderPiece, LineKind, SegColor, TranscriptLine, card_cell,
    compact_args, line, preview, user_block,
};

/// Replays one session's model-visible context as transcript lines.
///
/// Two-pass scan: the first pass indexes every durable `ToolResult` by its
/// `tool_call_id` so the second pass can fold each tool call together with
/// its outcome and derive a display projection for it. `AssistantToolCalls`
/// messages become one card (single call) or a concurrent tree (batch > 1);
/// the standalone `ToolResult` messages they absorbed emit nothing here.
pub(crate) fn history_lines(view: &DurableSessionView) -> Vec<TranscriptLine> {
    let mut outcomes: HashMap<&str, &FrontendToolResultOutcome> = HashMap::new();
    for message in &view.messages {
        if let FrontendContextMessage::ToolResult {
            tool_call_id, outcome, ..
        } = message
        {
            outcomes.insert(tool_call_id.as_str(), outcome);
        }
    }
    let mut lines = Vec::new();
    for message in &view.messages {
        match message {
            FrontendContextMessage::AssistantToolCalls {
                blocks,
                tool_batch_id,
            } => lines.extend(tool_call_cards(blocks, tool_batch_id, &outcomes)),
            FrontendContextMessage::ToolResult { .. } => {}
            _ => lines.extend(message_lines(message)),
        }
    }
    lines
}

/// Condensed preview for the session picker: the opening lines of the
/// session, truncated with a marker when more follows. Uses a compact
/// text-only projection (not the card form) so the picker stays scannable.
pub(crate) fn preview_lines(view: &DurableSessionView, max_lines: usize) -> Vec<String> {
    let lines = preview_text_lines(view);
    if lines.is_empty() {
        return vec!["(empty session)".to_owned()];
    }
    let visible: Vec<&str> = lines.iter().map(String::as_str).filter(|text| !text.is_empty()).collect();
    if visible.is_empty() {
        return vec!["(empty session)".to_owned()];
    }
    let mut texts: Vec<String> = visible
        .iter()
        .take(max_lines)
        .map(|text| preview(text, 120))
        .collect();
    if visible.len() > max_lines
        && let Some(last) = texts.last_mut()
    {
        *last = "...".to_owned();
    }
    texts
}

/// Compact text-only replay for the picker preview. Tool calls render the
/// older `▸ name args` / `└ ok · content` summary; everything else mirrors
/// `history_lines` minus the card structure.
fn preview_text_lines(view: &DurableSessionView) -> Vec<String> {
    let mut texts = Vec::new();
    for message in &view.messages {
        match message {
            FrontendContextMessage::Summary { text } => {
                for row in text.split('\n') {
                    texts.push(format!("[summary] {row}"));
                }
            }
            FrontendContextMessage::User { parts } => {
                for part in parts {
                    match part {
                        FrontendUserPart::Text(text) => texts.push(preview(text, 200)),
                        FrontendUserPart::Image { media_type, bytes } => {
                            texts.push(format!("[image {media_type}, {} bytes]", bytes.len()));
                        }
                    }
                }
            }
            FrontendContextMessage::Assistant { blocks } => {
                for block in blocks {
                    if let FrontendAssistantBlock::Text { text } = block {
                        texts.push(text.clone());
                    }
                }
            }
            FrontendContextMessage::AssistantToolCalls { blocks, .. } => {
                for block in blocks {
                    if let FrontendAssistantBlock::ToolCall {
                        name, arguments, ..
                    } = block
                    {
                        texts.push(format!("▸ {name}  {}", compact_args(arguments)));
                    }
                }
            }
            FrontendContextMessage::ToolResult { outcome, .. } => {
                texts.push(format!("  └ {}", preview_outcome_text(outcome)));
            }
        }
    }
    texts
}

fn preview_outcome_text(outcome: &FrontendToolResultOutcome) -> String {
    match outcome {
        FrontendToolResultOutcome::Success { content } => format!("ok · {}", preview(content, 80)),
        FrontendToolResultOutcome::Error { code, message } => {
            format!("error {code} · {}", preview(message, 80))
        }
        FrontendToolResultOutcome::Cancelled => "cancelled (never executed)".to_owned(),
        FrontendToolResultOutcome::Interrupted => "interrupted (execution state unknown)".to_owned(),
    }
}

fn message_lines(message: &FrontendContextMessage) -> Vec<TranscriptLine> {
    match message {
        FrontendContextMessage::Summary { text } => text
            .split('\n')
            .map(|text| line(LineKind::Meta, format!("[summary] {text}")))
            .collect(),
        FrontendContextMessage::User { parts } => user_block(parts.iter().map(user_part_text)),
        FrontendContextMessage::Assistant { blocks } => assistant_block_lines(blocks, false),
        // AssistantToolCalls / ToolResult are folded into cards by
        // `history_lines`; reaching here would mean a stray message.
        FrontendContextMessage::AssistantToolCalls { .. } | FrontendContextMessage::ToolResult { .. } => Vec::new(),
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

/// Builds the card lines for one `AssistantToolCalls` batch: every `ToolCall`
/// block in source order is paired with its durable outcome (looked up by
/// `tool_call_id`), its display is derived, and the result is rendered
/// through `default_card`. A batch of one renders a single card; a batch of
/// many renders the concurrent tree (parent header + child mini-card rows).
///
/// Text blocks interleaved with the tool calls are preserved as Answer lines
/// (the model's prose between tool calls is part of the replay).
fn tool_call_cards(
    blocks: &[FrontendAssistantBlock],
    _batch_id: &str,
    outcomes: &HashMap<&str, &FrontendToolResultOutcome>,
) -> Vec<TranscriptLine> {
    let mut tool_calls: Vec<(&FrontendAssistantBlock, &str, &str, &str)> = Vec::new();
    for block in blocks {
        if let FrontendAssistantBlock::ToolCall {
            id,
            name,
            arguments,
        } = block
        {
            tool_calls.push((block, id.as_str(), name.as_str(), arguments.as_str()));
        }
    }
    // If there are no tool calls at all, the message was pure prose: emit the
    // text blocks as Answer lines (mirrors the non-batch Assistant case).
    if tool_calls.is_empty() {
        return assistant_block_lines(blocks, false);
    }

    // Emit interleaved text blocks up front? No — the design keeps the batch
    // block order; text blocks are emitted in their position. We rebuild the
    // full block order so prose stays where the model placed it.
    let mut lines = Vec::new();
    for block in blocks {
        match block {
            FrontendAssistantBlock::Text { text } => {
                lines.push(line(LineKind::Answer, text.clone()));
            }
            FrontendAssistantBlock::ToolCall {
                id,
                name,
                arguments,
            } => {
                let outcome = outcomes.get(id.as_str()).copied();
                let card = replay_tool_card(name, arguments, outcome);
                if tool_calls.len() == 1 {
                    lines.extend(card);
                } else {
                    // For the tree form, fold each tool call into its single
                    // header cell; the parent tree header wraps them below.
                    // `default_card` already returns one header (+ optional
                    // body) line per call; we collect them and wrap.
                    lines.extend(card);
                }
            }
        }
    }
    if tool_calls.len() > 1 {
        // Wrap the per-call header cells in a concurrent-tree parent cell that
        // precedes them. The parent header carries the batch count and the
        // aggregate status; child rows reuse the per-call cards below it.
        let tree_parent = tree_parent_cell(tool_calls.len(), &tool_calls, outcomes);
        let mut ordered = Vec::with_capacity(lines.len() + 1);
        ordered.push(tree_parent);
        ordered.extend(lines);
        return ordered;
    }
    lines
}

/// One replayed tool call as a `default_card` (single) or a stopped cell
/// (Cancelled / Interrupted). Unknown tools and Error outcomes degrade to a
/// status-only card via `default_card`'s `display=None` / Error branch.
fn replay_tool_card(
    tool_name: &str,
    arguments: &str,
    outcome: Option<&FrontendToolResultOutcome>,
) -> Vec<TranscriptLine> {
    let Some(outcome) = outcome else {
        // No durable result recorded for this call id: render a done card
        // with no display (the session store lost the result).
        return tool_card::default_card(
            tool_name,
            arguments,
            &FrontendToolResult::Success {
                content: String::new(),
            },
            None,
            None,
        );
    };
    match outcome {
        FrontendToolResultOutcome::Success { content } => {
            let display = derive_display_for_replay(tool_name, arguments, outcome);
            tool_card::default_card(
                tool_name,
                arguments,
                &FrontendToolResult::Success {
                    content: content.clone(),
                },
                display.as_ref(),
                None,
            )
        }
        FrontendToolResultOutcome::Error { code, message } => tool_card::default_card(
            tool_name,
            arguments,
            &FrontendToolResult::Error {
                code: code.clone(),
                message: message.clone(),
            },
            None,
            None,
        ),
        FrontendToolResultOutcome::Cancelled => {
            vec![tool_card::stopped_cell(tool_name, arguments, "✗ cancelled")]
        }
        FrontendToolResultOutcome::Interrupted => {
            vec![tool_card::stopped_cell(tool_name, arguments, "✗ interrupted")]
        }
    }
}

/// The concurrent-tree parent cell for a replayed batch: a `▎ Parallel Task
/// (N operations)` header with the aggregate status (✓ done / ✗ failed),
/// no body. Children are the per-call cards that follow it.
fn tree_parent_cell(
    total: usize,
    tool_calls: &[(&FrontendAssistantBlock, &str, &str, &str)],
    outcomes: &HashMap<&str, &FrontendToolResultOutcome>,
) -> TranscriptLine {
    let any_failed = tool_calls.iter().any(|(_, id, _, _)| {
        outcomes.get(id).is_some_and(|outcome| {
            matches!(
                outcome,
                FrontendToolResultOutcome::Error { .. }
                    | FrontendToolResultOutcome::Cancelled
                    | FrontendToolResultOutcome::Interrupted
            )
        })
    });
    let (bar, status) = if any_failed {
        (SegColor::Red, "✗ failed")
    } else {
        (SegColor::Green, "✓ done")
    };
    let header = CardHeader {
        bar: HeaderPiece {
            text: "▎".to_owned(),
            color: bar,
            bold: false,
        },
        action: HeaderPiece {
            text: format!("Parallel Task ({total} operations)"),
            color: SegColor::Gray,
            bold: true,
        },
        target: None,
        stats: None,
        status: HeaderPiece {
            text: status.to_owned(),
            color: bar,
            bold: false,
        },
        time: None,
    };
    // No body on the parent; children are separate cells below it. We use a
    // card_cell with an empty body so the renderer treats it as a single
    // header cell (no fold bar).
    card_cell(
        header,
        CardBody {
            lines: Vec::new(),
            threshold: usize::MAX,
            fold_default_collapsed: false,
            fold_count: 0,
            fold_label: String::new(),
            fold_hint: false,
            fold_all: false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::support::{empty_session_view, image_session_view, session_view};

    #[test]
    fn history_renders_user_assistant_and_tool_cards() {
        let view = session_view("s-1");
        let lines = history_lines(&view);
        // The fixture's `read_file` is not one of the six standard tools, so
        // the display derivation returns None and the card degrades to a
        // status-only header cell (empty text, header struct).
        let user_texts: Vec<&str> = lines
            .iter()
            .filter(|line| line.kind == LineKind::User)
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(user_texts, ["", "count the files", ""]);
        let answer_texts: Vec<&str> = lines
            .iter()
            .filter(|line| line.kind == LineKind::Answer)
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(answer_texts, ["one file"]);
        let card_headers: Vec<&crate::app::transcript::CardHeader> =
            lines.iter().filter_map(|line| line.header.as_ref()).collect();
        assert_eq!(card_headers.len(), 1, "one tool call renders one card header");
        assert_eq!(card_headers[0].action.text, "read_file");
        assert_eq!(card_headers[0].status.text, "✓ done");
    }

    #[test]
    fn images_render_as_placeholder_text() {
        let view = image_session_view("s-img");
        let lines = history_lines(&view);
        let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(
            texts,
            ["", "look at this", "[image image/png, 4 bytes]", ""]
        );
    }

    #[test]
    fn tool_batch_renders_text_and_calls_in_block_order() {
        let lines = tool_call_cards(
            &[
                FrontendAssistantBlock::Text {
                    text: "let me look\nthen call".to_owned(),
                },
                FrontendAssistantBlock::ToolCall {
                    id: "c".to_owned(),
                    name: "read".to_owned(),
                    arguments: r#"{"path":"src/main.rs"}"#.to_owned(),
                },
                FrontendAssistantBlock::Text {
                    text: "after".to_owned(),
                },
            ],
            "batch",
            &HashMap::new(),
        );
        // With no outcome recorded, the read tool renders a status-only card;
        // the surrounding text blocks survive as Answer lines.
        let answer_texts: Vec<&str> = lines
            .iter()
            .filter(|line| line.kind == LineKind::Answer)
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(answer_texts, ["let me look\nthen call", "after"]);
        let tool_cells: Vec<&TranscriptLine> =
            lines.iter().filter(|line| line.kind == LineKind::Tool).collect();
        assert_eq!(tool_cells.len(), 1, "one tool call renders one card");
    }

    #[test]
    fn compacted_history_marks_the_summary_as_prior_context() {
        let lines = message_lines(&FrontendContextMessage::Summary {
            text: "earlier request\nearlier answer".to_owned(),
        });
        assert_eq!(
            lines,
            [
                line(LineKind::Meta, "[summary] earlier request"),
                line(LineKind::Meta, "[summary] earlier answer"),
            ]
        );
    }

    #[test]
    fn preview_truncates_with_a_marker() {
        let view = session_view("s-1");
        assert_eq!(
            preview_lines(&view, 2),
            ["count the files".to_owned(), "...".to_owned()]
        );
        let empty = empty_session_view("s-empty");
        assert_eq!(preview_lines(&empty, 4), ["(empty session)".to_owned()]);
    }

    #[test]
    fn standard_read_tool_renders_a_card_with_subject_and_count() {
        let view = DurableSessionView {
            session_id: "s-read".to_owned(),
            title: None,
            revision: 1,
            messages: vec![
                FrontendContextMessage::AssistantToolCalls {
                    tool_batch_id: "b".to_owned(),
                    blocks: vec![FrontendAssistantBlock::ToolCall {
                        id: "c".to_owned(),
                        name: "read".to_owned(),
                        arguments: r#"{"path":"src/main.rs"}"#.to_owned(),
                    }],
                },
                FrontendContextMessage::ToolResult {
                    tool_call_id: "c".to_owned(),
                    outcome: FrontendToolResultOutcome::Success {
                        content: "    1|fn main() {}".to_owned(),
                    },
                },
            ],
            open_turns: Vec::new(),
            settled_turn_boundaries: Vec::new(),
            latest_compaction_boundary: None,
            usage: None,
        };
        let lines = history_lines(&view);
        let tool_cells: Vec<&TranscriptLine> =
            lines.iter().filter(|line| line.kind == LineKind::Tool).collect();
        assert_eq!(tool_cells.len(), 1);
        let header = tool_cells[0].header.as_ref().expect("card header");
        assert_eq!(header.action.text, "Read");
        let target = header.target.as_ref().expect("subject is the target");
        assert_eq!(target.text, "src/main.rs");
        assert_eq!(header.status.text, "✓ done");
        assert_eq!(header.status.color, SegColor::Green);
        // No body for a read (body kind "none").
        assert!(tool_cells[0].body.is_none());
    }

    #[test]
    fn error_outcome_renders_a_red_failed_card() {
        let view = DurableSessionView {
            session_id: "s-err".to_owned(),
            title: None,
            revision: 1,
            messages: vec![
                FrontendContextMessage::AssistantToolCalls {
                    tool_batch_id: "b".to_owned(),
                    blocks: vec![FrontendAssistantBlock::ToolCall {
                        id: "c".to_owned(),
                        name: "read".to_owned(),
                        arguments: r#"{"path":"missing"}"#.to_owned(),
                    }],
                },
                FrontendContextMessage::ToolResult {
                    tool_call_id: "c".to_owned(),
                    outcome: FrontendToolResultOutcome::Error {
                        code: "not_found".to_owned(),
                        message: "no such file".to_owned(),
                    },
                },
            ],
            open_turns: Vec::new(),
            settled_turn_boundaries: Vec::new(),
            latest_compaction_boundary: None,
            usage: None,
        };
        let lines = history_lines(&view);
        // An Error card is a header cell plus a red failure line.
        let tool_lines: Vec<&TranscriptLine> =
            lines.iter().filter(|line| line.kind == LineKind::Tool).collect();
        assert_eq!(tool_lines.len(), 2, "error card is header + failure line");
        let header = tool_lines[0].header.as_ref().expect("card header");
        assert_eq!(header.bar.color, SegColor::Red);
        assert_eq!(header.status.text, "✗ failed");
        assert_eq!(header.status.color, SegColor::Red);
        assert!(tool_lines[1].text.contains("not_found"), "failure line cites the code");
    }

    #[test]
    fn cancelled_outcome_renders_a_red_cancelled_card() {
        let view = DurableSessionView {
            session_id: "s-cancel".to_owned(),
            title: None,
            revision: 1,
            messages: vec![
                FrontendContextMessage::AssistantToolCalls {
                    tool_batch_id: "b".to_owned(),
                    blocks: vec![FrontendAssistantBlock::ToolCall {
                        id: "c".to_owned(),
                        name: "read".to_owned(),
                        arguments: r#"{"path":"x"}"#.to_owned(),
                    }],
                },
                FrontendContextMessage::ToolResult {
                    tool_call_id: "c".to_owned(),
                    outcome: FrontendToolResultOutcome::Cancelled,
                },
            ],
            open_turns: Vec::new(),
            settled_turn_boundaries: Vec::new(),
            latest_compaction_boundary: None,
            usage: None,
        };
        let lines = history_lines(&view);
        let card_headers: Vec<&crate::app::transcript::CardHeader> =
            lines.iter().filter_map(|line| line.header.as_ref()).collect();
        assert_eq!(card_headers.len(), 1);
        let header = card_headers[0];
        assert_eq!(header.bar.color, SegColor::Red);
        assert_eq!(header.status.text, "✗ cancelled");
        assert_eq!(header.status.color, SegColor::Red);
        assert!(lines.iter().all(|line| line.body.is_none()), "no body for a cancelled card");
    }

    #[test]
    fn interrupted_outcome_renders_a_red_interrupted_card() {
        let view = DurableSessionView {
            session_id: "s-int".to_owned(),
            title: None,
            revision: 1,
            messages: vec![
                FrontendContextMessage::AssistantToolCalls {
                    tool_batch_id: "b".to_owned(),
                    blocks: vec![FrontendAssistantBlock::ToolCall {
                        id: "c".to_owned(),
                        name: "shell".to_owned(),
                        arguments: r#"{"command":"x"}"#.to_owned(),
                    }],
                },
                FrontendContextMessage::ToolResult {
                    tool_call_id: "c".to_owned(),
                    outcome: FrontendToolResultOutcome::Interrupted,
                },
            ],
            open_turns: Vec::new(),
            settled_turn_boundaries: Vec::new(),
            latest_compaction_boundary: None,
            usage: None,
        };
        let lines = history_lines(&view);
        let card_headers: Vec<&crate::app::transcript::CardHeader> =
            lines.iter().filter_map(|line| line.header.as_ref()).collect();
        assert_eq!(card_headers.len(), 1);
        let header = card_headers[0];
        assert_eq!(header.status.text, "✗ interrupted");
        assert_eq!(header.status.color, SegColor::Red);
    }

    #[test]
    fn concurrent_batch_renders_a_tree_parent_followed_by_child_cards() {
        let view = DurableSessionView {
            session_id: "s-batch".to_owned(),
            title: None,
            revision: 1,
            messages: vec![
                FrontendContextMessage::AssistantToolCalls {
                    tool_batch_id: "b".to_owned(),
                    blocks: vec![
                        FrontendAssistantBlock::ToolCall {
                            id: "c1".to_owned(),
                            name: "read".to_owned(),
                            arguments: r#"{"path":"a"}"#.to_owned(),
                        },
                        FrontendAssistantBlock::ToolCall {
                            id: "c2".to_owned(),
                            name: "grep".to_owned(),
                            arguments: r#"{"pattern":"x"}"#.to_owned(),
                        },
                    ],
                },
                FrontendContextMessage::ToolResult {
                    tool_call_id: "c1".to_owned(),
                    outcome: FrontendToolResultOutcome::Success {
                        content: "    1|a".to_owned(),
                    },
                },
                FrontendContextMessage::ToolResult {
                    tool_call_id: "c2".to_owned(),
                    outcome: FrontendToolResultOutcome::Success {
                        content: "a:1: x".to_owned(),
                    },
                },
            ],
            open_turns: Vec::new(),
            settled_turn_boundaries: Vec::new(),
            latest_compaction_boundary: None,
            usage: None,
        };
        let lines = history_lines(&view);
        let card_headers: Vec<&crate::app::transcript::CardHeader> =
            lines.iter().filter_map(|line| line.header.as_ref()).collect();
        // Parent tree header + two child card headers.
        assert_eq!(card_headers.len(), 3);
        assert_eq!(card_headers[0].action.text, "Parallel Task (2 operations)");
        assert_eq!(card_headers[0].status.text, "✓ done");
        assert_eq!(card_headers[0].status.color, SegColor::Green);
        // Children: read card then grep card, in source order.
        assert_eq!(card_headers[1].action.text, "Read");
        assert_eq!(card_headers[2].action.text, "Grep");
    }

    #[test]
    fn concurrent_batch_with_one_failure_marks_the_parent_failed() {
        let view = DurableSessionView {
            session_id: "s-batch-fail".to_owned(),
            title: None,
            revision: 1,
            messages: vec![
                FrontendContextMessage::AssistantToolCalls {
                    tool_batch_id: "b".to_owned(),
                    blocks: vec![
                        FrontendAssistantBlock::ToolCall {
                            id: "c1".to_owned(),
                            name: "read".to_owned(),
                            arguments: r#"{"path":"a"}"#.to_owned(),
                        },
                        FrontendAssistantBlock::ToolCall {
                            id: "c2".to_owned(),
                            name: "read".to_owned(),
                            arguments: r#"{"path":"b"}"#.to_owned(),
                        },
                    ],
                },
                FrontendContextMessage::ToolResult {
                    tool_call_id: "c1".to_owned(),
                    outcome: FrontendToolResultOutcome::Success {
                        content: "ok".to_owned(),
                    },
                },
                FrontendContextMessage::ToolResult {
                    tool_call_id: "c2".to_owned(),
                    outcome: FrontendToolResultOutcome::Error {
                        code: "not_found".to_owned(),
                        message: "missing".to_owned(),
                    },
                },
            ],
            open_turns: Vec::new(),
            settled_turn_boundaries: Vec::new(),
            latest_compaction_boundary: None,
            usage: None,
        };
        let lines = history_lines(&view);
        let card_headers: Vec<&crate::app::transcript::CardHeader> =
            lines.iter().filter_map(|line| line.header.as_ref()).collect();
        let parent = card_headers[0];
        assert_eq!(parent.action.text, "Parallel Task (2 operations)");
        assert_eq!(parent.status.text, "✗ failed");
        assert_eq!(parent.status.color, SegColor::Red);
    }

    #[test]
    fn unknown_tool_renders_a_status_only_card() {
        let view = DurableSessionView {
            session_id: "s-unk".to_owned(),
            title: None,
            revision: 1,
            messages: vec![
                FrontendContextMessage::AssistantToolCalls {
                    tool_batch_id: "b".to_owned(),
                    blocks: vec![FrontendAssistantBlock::ToolCall {
                        id: "c".to_owned(),
                        name: "mystery".to_owned(),
                        arguments: r#"{"x":"y"}"#.to_owned(),
                    }],
                },
                FrontendContextMessage::ToolResult {
                    tool_call_id: "c".to_owned(),
                    outcome: FrontendToolResultOutcome::Success {
                        content: "ok".to_owned(),
                    },
                },
            ],
            open_turns: Vec::new(),
            settled_turn_boundaries: Vec::new(),
            latest_compaction_boundary: None,
            usage: None,
        };
        let lines = history_lines(&view);
        let card_headers: Vec<&crate::app::transcript::CardHeader> =
            lines.iter().filter_map(|line| line.header.as_ref()).collect();
        assert_eq!(card_headers.len(), 1);
        let header = card_headers[0];
        assert_eq!(header.action.text, "mystery");
        assert_eq!(header.status.text, "✓ done");
        assert_eq!(header.stats, None, "no stats for an unknown tool");
        assert!(lines.iter().all(|line| line.body.is_none()), "no body for an unknown tool");
    }
}
