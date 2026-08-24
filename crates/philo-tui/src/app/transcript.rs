//! Event-to-cell reducer for the scrollback transcript.
//!
//! [`Transcript::apply`] writes into a [`TranscriptStore`]: in-progress
//! Answer/Think is a real cell at its insertion point, not a second channel.
//! Per-op flags live here; `store.clear()` is the App's job on session switch.

use std::collections::HashMap;

use philo_agent_service::{
    FailureLineStyle, FrontendOperationEvent, retry_scheduled_lines, turn_failed_lines,
};

use super::cells::TranscriptStore;
use super::text;
use super::tool_card;

/// Information tier of the transcript (the TUI has no quiet tier; `Ctrl+O`
/// toggles between these two).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InfoLevel {
    #[default]
    Default,
    Verbose,
}

/// Semantic kind of one transcript line; the terminal shell maps kinds to
/// styles (colors are implementation freedom, kinds are the testable fact).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// The model's answer text.
    Answer,
    /// Visible reasoning.
    Reasoning,
    /// Tool progress and results.
    Tool,
    /// Notices: queueing, sealing, cancellation.
    Notice,
    /// Failures and the unconfirmed warning.
    Error,
    /// Terminal status lines and echoes.
    Meta,
    /// The user's submitted message, echoed into history.
    User,
}

/// One append-only transcript line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptLine {
    pub kind: LineKind,
    pub text: String,
}

pub(crate) fn line(kind: LineKind, text: impl Into<String>) -> TranscriptLine {
    TranscriptLine {
        kind,
        text: text.into(),
    }
}

/// One user turn: blank, `›` first row, hanging continuations, blank.
pub(crate) fn user_block(rows: impl IntoIterator<Item = String>) -> Vec<TranscriptLine> {
    let mut lines = vec![line(LineKind::User, "")];
    let mut first = true;
    for row in rows {
        if first {
            lines.push(line(LineKind::User, format!("› {row}")));
            first = false;
        } else {
            lines.push(line(LineKind::User, format!("  {row}")));
        }
    }
    lines.push(line(LineKind::User, ""));
    lines
}

/// What the user sees when an attachment or stale media result stops the send.
pub(crate) fn refusal_lines_for_restore(errors: &[String], restored: bool) -> Vec<TranscriptLine> {
    let mut lines: Vec<TranscriptLine> = errors
        .iter()
        .map(|error| line(LineKind::Error, format!("error: {error}")))
        .collect();
    let outcome = if restored {
        "the message was not sent; it is back in the input"
    } else {
        "the message was not sent; newer input was left unchanged"
    };
    lines.push(line(LineKind::Error, outcome));
    lines
}

/// Streaming projection state for one operation's events.
#[derive(Debug, Default)]
pub struct Transcript {
    think_header_written: bool,
    tool_batch_size: usize,
    tool_args: HashMap<usize, String>,
    /// `[ui].show_reasoning`: when off, reasoning deltas are dropped rather
    /// than rendered dim (the model still receives them).
    show_reasoning: bool,
    wrote_answer_this_call: bool,
}

impl Transcript {
    pub fn new(show_reasoning: bool) -> Self {
        Self {
            show_reasoning,
            ..Self::default()
        }
    }

    /// Applies `[ui].show_reasoning` from a config reload without rebuilding
    /// the rest of the transcript.
    pub fn set_show_reasoning(&mut self, show_reasoning: bool) {
        self.show_reasoning = show_reasoning;
        if !show_reasoning {
            self.think_header_written = false;
        }
    }

    /// Projects one event into the ordered store. Resets per-op flags on
    /// settle; the App clears the store on session switch.
    pub fn apply(
        &mut self,
        store: &mut TranscriptStore,
        event: &FrontendOperationEvent,
        level: InfoLevel,
    ) {
        let verbose = level == InfoLevel::Verbose;
        match event {
            FrontendOperationEvent::TextDelta { delta } => self.apply_text_delta(store, delta),
            FrontendOperationEvent::ReasoningDelta { .. } if !self.show_reasoning => {}
            FrontendOperationEvent::ReasoningDelta { text, .. } => {
                self.apply_reasoning_delta(store, text)
            }
            FrontendOperationEvent::OperationQueued { .. } => {
                store.push_closed([line(LineKind::Notice, "queued behind the active turn")]);
            }
            FrontendOperationEvent::OperationStarted { operation_id } => {
                if verbose {
                    store.push_closed([line(
                        LineKind::Meta,
                        format!("operation {operation_id} started"),
                    )]);
                }
            }
            FrontendOperationEvent::TurnStarted { turn_id } => {
                if verbose {
                    store.push_closed([line(LineKind::Meta, format!("turn {turn_id} started"))]);
                }
            }
            FrontendOperationEvent::ModelCallStarted { model_call_id } => {
                store.close_open();
                self.wrote_answer_this_call = false;
                self.think_header_written = false;
                if verbose {
                    store
                        .push_closed([line(LineKind::Meta, format!("model call {model_call_id}"))]);
                }
            }
            FrontendOperationEvent::ModelResponseStarted {
                response_model,
                response_id,
                ..
            } => {
                if verbose {
                    store.push_closed([line(
                        LineKind::Meta,
                        format!(
                            "model response: model={} id={}",
                            response_model.as_deref().unwrap_or("-"),
                            response_id.as_deref().unwrap_or("-"),
                        ),
                    )]);
                }
            }
            FrontendOperationEvent::ModelUsageUpdated { .. } => {}
            FrontendOperationEvent::ToolBatchRequested { call_count, .. } => {
                store.close_open();
                self.tool_batch_size = *call_count;
                self.tool_args.clear();
            }
            FrontendOperationEvent::ToolExecutionStarted {
                arguments, index, ..
            } => {
                self.tool_args.insert(*index, arguments.clone());
            }
            FrontendOperationEvent::ToolExecutionProgress { .. } => {}
            FrontendOperationEvent::ToolExecutionCompleted {
                tool_name,
                result,
                display,
                index,
                ..
            } => {
                store.close_open();
                let arguments = self.tool_args.remove(index).unwrap_or_default();
                let card = if verbose {
                    tool_card::verbose_card(
                        tool_name,
                        *index,
                        self.tool_batch_size,
                        &arguments,
                        result,
                        display.as_ref(),
                    )
                } else {
                    tool_card::default_card(tool_name, &arguments, result, display.as_ref())
                };
                store.push_closed(card);
            }
            FrontendOperationEvent::AssistantMessageCompleted { content, .. } => {
                store.close_open();
                if !self.wrote_answer_this_call && !content.is_empty() {
                    store.push_closed([line(LineKind::Answer, content)]);
                }
            }
            FrontendOperationEvent::PriorTurnSealed { turn_id } => {
                if verbose {
                    store.push_closed([line(
                        LineKind::Notice,
                        format!(
                            "previous turn {turn_id} did not end cleanly and was sealed; \
                             its tool calls may have executed without recorded results"
                        ),
                    )]);
                } else {
                    store.push_closed([line(
                        LineKind::Notice,
                        "previous turn did not end cleanly and was sealed; its tool \
                         calls may have executed without recorded results",
                    )]);
                }
            }
            FrontendOperationEvent::ContextCompactionStarted => {
                if verbose {
                    store.push_closed([line(LineKind::Notice, "compacting context...")]);
                }
            }
            FrontendOperationEvent::ContextCompactionCompleted { covers_up_to } => {
                let text = if verbose {
                    format!("context compacted through {covers_up_to}")
                } else {
                    "context compacted".to_owned()
                };
                store.push_closed([line(LineKind::Meta, text)]);
            }
            FrontendOperationEvent::ContextCompactionFailed { message } => {
                store.push_closed([line(
                    LineKind::Error,
                    format!("compaction failed: {message}; continuing without compaction"),
                )]);
            }
            FrontendOperationEvent::ModelRetryScheduled {
                attempt,
                max_retries,
                delay_ms,
                failure,
                ..
            } => {
                // The failed attempt's streamed text is discarded; close the
                // open view and reset per-call flags so the retry renders
                // cleanly. Wording comes from the service's shared template.
                store.close_open();
                self.wrote_answer_this_call = false;
                self.think_header_written = false;
                let lines = retry_scheduled_lines(failure, *attempt, *max_retries, *delay_ms);
                let rendered: Vec<TranscriptLine> = lines
                    .iter()
                    .map(|rendered| {
                        line(
                            match rendered.style {
                                FailureLineStyle::Error => LineKind::Error,
                                FailureLineStyle::Meta => LineKind::Meta,
                            },
                            rendered.text.clone(),
                        )
                    })
                    .collect();
                store.push_closed(rendered);
            }
            FrontendOperationEvent::CancellationRequested { reason, .. } => {
                store.close_open();
                if verbose {
                    store.push_closed([line(
                        LineKind::Notice,
                        format!("cancelling ({})...", reason_text(reason)),
                    )]);
                }
            }
            FrontendOperationEvent::TurnCancelled { reason, .. } => {
                store.close_open();
                store.push_closed([line(
                    LineKind::Notice,
                    format!("turn cancelled ({})", reason_text(reason)),
                )]);
            }
            FrontendOperationEvent::TurnFailed { failure, .. } => {
                store.close_open();
                // Three-tier rendering (summary / tags / detail) with
                // wording supplied by the service's shared template.
                let lines = turn_failed_lines(failure);
                let rendered: Vec<TranscriptLine> = lines
                    .iter()
                    .map(|rendered| {
                        line(
                            match rendered.style {
                                FailureLineStyle::Error => LineKind::Error,
                                FailureLineStyle::Meta => LineKind::Meta,
                            },
                            rendered.text.clone(),
                        )
                    })
                    .collect();
                store.push_closed(rendered);
            }
            FrontendOperationEvent::OperationSettled {
                status, durability, ..
            } => {
                store.close_open();
                if status.eq_ignore_ascii_case("failed") {
                    store.push_closed([line(LineKind::Meta, "done (failed)")]);
                } else if status.eq_ignore_ascii_case("cancelled") {
                    store.push_closed([line(LineKind::Meta, "done (cancelled)")]);
                }
                if durability.eq_ignore_ascii_case("unconfirmed") {
                    store.push_closed([line(
                        LineKind::Error,
                        "WARNING: settlement durability UNCONFIRMED - the session may not \
                         have durably recorded this outcome",
                    )]);
                }
                self.clear_ephemeral();
            }
        }
    }

    fn apply_text_delta(&mut self, store: &mut TranscriptStore, delta: &str) {
        if store.open_kind() == Some(LineKind::Reasoning) {
            store.close_open();
            self.think_header_written = false;
        }
        if store.open_kind() != Some(LineKind::Answer) {
            store.begin(LineKind::Answer, "");
        }
        store.write_open(delta);
        self.wrote_answer_this_call = true;
    }

    fn apply_reasoning_delta(&mut self, store: &mut TranscriptStore, text: &str) {
        if store.open_kind() == Some(LineKind::Answer) {
            store.close_open();
        }
        if !self.think_header_written {
            store.push_closed([line(LineKind::Reasoning, "think")]);
            self.think_header_written = true;
        }

        let mut raw = String::new();
        if store.open_kind() == Some(LineKind::Reasoning) {
            let existing = store.take_open().expect("open reasoning cell");
            raw.push_str(existing.text.strip_prefix("  ").unwrap_or(&existing.text));
        }
        raw.push_str(text);

        while let Some(newline) = raw.find('\n') {
            let mut completed: String = raw.drain(..=newline).collect();
            completed.pop();
            if !completed.is_empty() {
                store.push_closed([line(LineKind::Reasoning, format!("  {completed}"))]);
            }
        }
        if !raw.is_empty() {
            store.begin(LineKind::Reasoning, format!("  {raw}"));
        }
    }

    fn clear_ephemeral(&mut self) {
        self.tool_batch_size = 0;
        self.tool_args.clear();
        self.think_header_written = false;
        self.wrote_answer_this_call = false;
    }
}

fn reason_text(reason: &str) -> String {
    match reason {
        "User" | "user" => "user".to_owned(),
        "Timeout" | "timeout" => "timeout".to_owned(),
        "Abandoned" | "abandoned" => "abandoned".to_owned(),
        other => other.to_ascii_lowercase(),
    }
}

/// Single-line preview bounded by terminal cells without splitting graphemes.
pub(crate) fn preview(text: &str, max_width: usize) -> String {
    let flat = text.replace(['\n', '\r'], " ");
    let flat = flat.trim();
    text::truncate(flat, max_width)
}

/// Flattens a JSON-looking argument object into `key: value` pairs.
pub(crate) fn compact_args(raw: &str) -> String {
    let trimmed = raw.trim();
    let Some(inner) = trimmed
        .strip_prefix('{')
        .and_then(|text| text.strip_suffix('}'))
    else {
        return preview(trimmed, 80);
    };
    let compact = inner.replace('"', "").replace(',', "  ").replace(':', ": ");
    preview(&compact, 80)
}

#[cfg(test)]
mod tests {
    use philo_agent_service::{
        FrontendOperationEvent, FrontendTokenUsage, FrontendToolDisplay, FrontendToolResult,
    };

    use super::*;

    fn apply_all(
        events: &[FrontendOperationEvent],
        level: InfoLevel,
        show_reasoning: bool,
    ) -> TranscriptStore {
        let mut transcript = Transcript::new(show_reasoning);
        let mut store = TranscriptStore::new();
        for event in events {
            transcript.apply(&mut store, event, level);
        }
        store
    }

    fn format_cells(store: &TranscriptStore) -> String {
        store
            .cells()
            .iter()
            .map(|line| {
                let text = line.text.replace('\n', "\\n");
                format!("{:?}: {text}", line.kind)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn event_sequence() -> Vec<FrontendOperationEvent> {
        vec![
            FrontendOperationEvent::OperationStarted {
                operation_id: "op-1".to_owned(),
            },
            FrontendOperationEvent::PriorTurnSealed {
                turn_id: "old-turn".to_owned(),
            },
            FrontendOperationEvent::TurnStarted {
                turn_id: "turn-1".to_owned(),
            },
            FrontendOperationEvent::ModelCallStarted {
                model_call_id: "call-1".to_owned(),
            },
            FrontendOperationEvent::ReasoningDelta {
                model_call_id: "call-1".to_owned(),
                text: "checking workspace\n".to_owned(),
            },
            FrontendOperationEvent::TextDelta {
                delta: "answer line\nfinal".to_owned(),
            },
            FrontendOperationEvent::ModelUsageUpdated {
                model_call_id: "call-1".to_owned(),
                usage: FrontendTokenUsage {
                    input_tokens: Some(12),
                    output_tokens: Some(7),
                    ..FrontendTokenUsage::default()
                },
            },
            FrontendOperationEvent::ToolBatchRequested {
                tool_batch_id: "batch-1".to_owned(),
                call_count: 1,
            },
            FrontendOperationEvent::ToolExecutionStarted {
                tool_batch_id: "batch-1".to_owned(),
                tool_call_id: "tool-1".to_owned(),
                index: 0,
                tool_name: "read_file".to_owned(),
                arguments: "{\"path\":\"src/main.rs\"}".to_owned(),
            },
            FrontendOperationEvent::ToolExecutionCompleted {
                tool_batch_id: "batch-1".to_owned(),
                tool_call_id: "tool-1".to_owned(),
                index: 0,
                tool_name: "read_file".to_owned(),
                result: FrontendToolResult::Success {
                    content: "fn main() {}".to_owned(),
                },
                display: Some(FrontendToolDisplay {
                    detail: "read 12 bytes".to_owned(),
                    facts: vec![("bytes".to_owned(), "12".to_owned())],
                }),
            },
            FrontendOperationEvent::CancellationRequested {
                operation_id: "op-1".to_owned(),
                reason: "Timeout".to_owned(),
            },
            FrontendOperationEvent::TurnCancelled {
                turn_id: "turn-1".to_owned(),
                reason: "Timeout".to_owned(),
            },
            FrontendOperationEvent::OperationSettled {
                operation_id: "op-1".to_owned(),
                session_id: "s-1".to_owned(),
                status: "Cancelled".to_owned(),
                durability: "Confirmed".to_owned(),
                session_revision: philo_agent_service::SettlementRevision::Unchanged,
            },
        ]
    }

    fn render(level: InfoLevel) -> String {
        let store = apply_all(&event_sequence(), level, true);
        format!(
            "lines:\n{}\nopen: {:?}",
            format_cells(&store),
            store.open_index()
        )
    }

    #[test]
    fn default_render_snapshot() {
        crate::tests::assert_tui_snapshot!("transcript_default", render(InfoLevel::Default));
    }

    #[test]
    fn verbose_render_snapshot() {
        crate::tests::assert_tui_snapshot!("transcript_verbose", render(InfoLevel::Verbose));
    }

    #[test]
    fn think_stays_line_oriented_with_an_open_remainder() {
        let mut transcript = Transcript::new(true);
        let mut store = TranscriptStore::new();
        transcript.apply(
            &mut store,
            &FrontendOperationEvent::ReasoningDelta {
                model_call_id: "call-1".to_owned(),
                text: "hello".to_owned(),
            },
            InfoLevel::Default,
        );
        assert_eq!(
            store.cells(),
            [
                line(LineKind::Reasoning, "think"),
                line(LineKind::Reasoning, "  hello"),
            ]
        );
        assert_eq!(store.open_index(), Some(1));

        transcript.apply(
            &mut store,
            &FrontendOperationEvent::ReasoningDelta {
                model_call_id: "call-1".to_owned(),
                text: " world\nmore".to_owned(),
            },
            InfoLevel::Default,
        );
        assert_eq!(
            store.cells(),
            [
                line(LineKind::Reasoning, "think"),
                line(LineKind::Reasoning, "  hello world"),
                line(LineKind::Reasoning, "  more"),
            ]
        );
        assert_eq!(store.open_index(), Some(2));
    }

    #[test]
    fn reasoning_can_be_switched_off_entirely() {
        let mut transcript = Transcript::new(false);
        let mut store = TranscriptStore::new();
        transcript.apply(
            &mut store,
            &FrontendOperationEvent::ReasoningDelta {
                model_call_id: "call-1".to_owned(),
                text: "thinking\n".to_owned(),
            },
            InfoLevel::Verbose,
        );
        assert!(store.is_empty(), "no reasoning reaches the transcript");
        assert!(!store.has_open());
    }

    #[test]
    fn default_tool_started_is_transient_and_completion_is_bounded() {
        let mut transcript = Transcript::new(true);
        let mut store = TranscriptStore::new();
        let started = FrontendOperationEvent::ToolExecutionStarted {
            tool_batch_id: "batch".to_owned(),
            tool_call_id: "tool".to_owned(),
            index: 0,
            tool_name: "读取文件".repeat(40),
            arguments: "{\"path\":\"src/main.rs\"}".to_owned(),
        };
        transcript.apply(&mut store, &started, InfoLevel::Default);
        assert!(
            store.is_empty(),
            "started belongs only to Activity in default mode"
        );

        let completed = FrontendOperationEvent::ToolExecutionCompleted {
            tool_batch_id: "batch".to_owned(),
            tool_call_id: "tool".to_owned(),
            index: 0,
            tool_name: "读取文件".repeat(40),
            result: FrontendToolResult::Success {
                content: "内容".repeat(100),
            },
            display: None,
        };
        transcript.apply(&mut store, &completed, InfoLevel::Default);
        assert_eq!(store.cells().len(), 1);
        assert!(text::width(&store.cells()[0].text) <= 120);
        assert!(
            !store.cells()[0].text.contains("内容"),
            "default cards must not dump model-facing content"
        );
    }

    #[test]
    fn tool_progress_never_writes_history() {
        let mut transcript = Transcript::new(true);
        let mut store = TranscriptStore::new();
        let progress = FrontendOperationEvent::ToolExecutionProgress {
            tool_batch_id: "batch".to_owned(),
            tool_call_id: "tool".to_owned(),
            index: 0,
            tail: "live output that must stay off scrollback".to_owned(),
        };
        transcript.apply(&mut store, &progress, InfoLevel::Default);
        assert!(store.is_empty());
        transcript.apply(&mut store, &progress, InfoLevel::Verbose);
        assert!(store.is_empty());
    }

    #[test]
    fn preamble_stays_before_tools_and_next_call_starts_a_new_answer() {
        let mut transcript = Transcript::new(true);
        let mut store = TranscriptStore::new();
        let events = [
            FrontendOperationEvent::TextDelta {
                delta: "I'll look".to_owned(),
            },
            FrontendOperationEvent::ToolBatchRequested {
                tool_batch_id: "batch".to_owned(),
                call_count: 1,
            },
            FrontendOperationEvent::ToolExecutionCompleted {
                tool_batch_id: "batch".to_owned(),
                tool_call_id: "tool".to_owned(),
                index: 0,
                tool_name: "read".to_owned(),
                result: FrontendToolResult::Success {
                    content: "ok".to_owned(),
                },
                display: None,
            },
            FrontendOperationEvent::ModelCallStarted {
                model_call_id: "call-2".to_owned(),
            },
            FrontendOperationEvent::TextDelta {
                delta: "done".to_owned(),
            },
        ];
        for event in &events {
            transcript.apply(&mut store, event, InfoLevel::Default);
        }

        let answers: Vec<&TranscriptLine> = store
            .cells()
            .iter()
            .filter(|cell| cell.kind == LineKind::Answer)
            .collect();
        assert_eq!(answers.len(), 2, "two answer cells, not concatenated");
        assert_eq!(answers[0].text, "I'll look");
        assert_eq!(answers[1].text, "done");

        let first_answer = store
            .cells()
            .iter()
            .position(|cell| cell.kind == LineKind::Answer)
            .expect("first answer");
        let first_tool = store
            .cells()
            .iter()
            .position(|cell| cell.kind == LineKind::Tool)
            .expect("tool card");
        let last_answer = store
            .cells()
            .iter()
            .rposition(|cell| cell.kind == LineKind::Answer)
            .expect("second answer");
        assert!(
            first_answer < first_tool,
            "tools must not appear before the first answer: {}",
            format_cells(&store)
        );
        assert!(
            first_tool < last_answer,
            "second answer follows the tool card"
        );
        assert_eq!(store.open_index(), Some(last_answer));
    }

    #[test]
    fn event_sequence_snapshot_covers_m11_and_dual_channel() {
        crate::tests::assert_tui_snapshot!(
            "transcript_event_sequence",
            event_sequence()
                .iter()
                .map(|event| format!("{event:?}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}
