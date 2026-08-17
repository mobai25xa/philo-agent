//! Event-to-cell reducer for the scrollback transcript.
//!
//! [`Transcript::apply`] writes into a [`TranscriptStore`]: in-progress
//! Answer/Think is a real cell at its insertion point, not a second channel.
//! Per-op flags live here; `store.clear()` is the App's job on session switch.

use std::collections::HashMap;

use philo_agent_runtime::{AgentEvent, CancelReason, OperationStatus, SettlementDurability};

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
    pub fn apply(&mut self, store: &mut TranscriptStore, event: &AgentEvent, level: InfoLevel) {
        let verbose = level == InfoLevel::Verbose;
        match event {
            AgentEvent::TextDelta { delta } => self.apply_text_delta(store, delta),
            AgentEvent::ReasoningDelta { .. } if !self.show_reasoning => {}
            AgentEvent::ReasoningDelta { text, .. } => self.apply_reasoning_delta(store, text),
            AgentEvent::OperationQueued { .. } => {
                store.push_closed([line(LineKind::Notice, "queued behind the active turn")]);
            }
            AgentEvent::OperationStarted { operation_id } => {
                if verbose {
                    store.push_closed([line(
                        LineKind::Meta,
                        format!("operation {operation_id} started"),
                    )]);
                }
            }
            AgentEvent::TurnStarted { turn_id } => {
                if verbose {
                    store.push_closed([line(LineKind::Meta, format!("turn {turn_id} started"))]);
                }
            }
            AgentEvent::ModelCallStarted { model_call_id } => {
                store.close_open();
                self.wrote_answer_this_call = false;
                self.think_header_written = false;
                if verbose {
                    store
                        .push_closed([line(LineKind::Meta, format!("model call {model_call_id}"))]);
                }
            }
            AgentEvent::ModelResponseStarted {
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
            AgentEvent::ModelUsageUpdated { .. } => {}
            AgentEvent::ToolBatchRequested { call_count, .. } => {
                store.close_open();
                self.tool_batch_size = *call_count;
                self.tool_args.clear();
            }
            AgentEvent::ToolExecutionStarted {
                arguments, index, ..
            } => {
                self.tool_args.insert(*index, arguments.clone());
            }
            AgentEvent::ToolExecutionProgress { .. } => {}
            AgentEvent::ToolExecutionCompleted {
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
            AgentEvent::AssistantMessageCompleted { message, .. } => {
                store.close_open();
                if !self.wrote_answer_this_call && !message.content().is_empty() {
                    store.push_closed([line(LineKind::Answer, message.content())]);
                }
            }
            AgentEvent::PriorTurnSealed { turn_id } => {
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
            AgentEvent::ContextCompactionStarted => {
                if verbose {
                    store.push_closed([line(LineKind::Notice, "compacting context...")]);
                }
            }
            AgentEvent::ContextCompactionCompleted { covers_up_to } => {
                let text = if verbose {
                    format!("context compacted through {covers_up_to}")
                } else {
                    "context compacted".to_owned()
                };
                store.push_closed([line(LineKind::Meta, text)]);
            }
            AgentEvent::ContextCompactionFailed { message } => {
                store.push_closed([line(
                    LineKind::Error,
                    format!("compaction failed: {message}; continuing without compaction"),
                )]);
            }
            AgentEvent::CancellationRequested { reason, .. } => {
                store.close_open();
                if verbose {
                    store.push_closed([line(
                        LineKind::Notice,
                        format!("cancelling ({})...", reason_text(*reason)),
                    )]);
                }
            }
            AgentEvent::TurnCancelled { reason, .. } => {
                store.close_open();
                store.push_closed([line(
                    LineKind::Notice,
                    format!("turn cancelled ({})", reason_text(*reason)),
                )]);
            }
            AgentEvent::TurnFailed { failure, .. } => {
                store.close_open();
                store.push_closed([line(
                    LineKind::Error,
                    format!("turn failed ({:?}): {}", failure.kind(), failure.message()),
                )]);
            }
            AgentEvent::OperationSettled {
                status, durability, ..
            } => {
                store.close_open();
                match status {
                    OperationStatus::Succeeded => {}
                    OperationStatus::Failed => {
                        store.push_closed([line(LineKind::Meta, "done (failed)")]);
                    }
                    OperationStatus::Cancelled => {
                        store.push_closed([line(LineKind::Meta, "done (cancelled)")]);
                    }
                }
                if *durability == SettlementDurability::Unconfirmed {
                    store.push_closed([line(
                        LineKind::Error,
                        "WARNING: settlement durability UNCONFIRMED - the session may not \
                         have durably recorded this outcome",
                    )]);
                }
                self.clear_ephemeral();
            }
            _ => {}
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

fn reason_text(reason: CancelReason) -> &'static str {
    match reason {
        CancelReason::User => "user",
        CancelReason::Timeout => "timeout",
        CancelReason::Abandoned => "abandoned",
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
    use super::*;
    use philo_agent_runtime::{
        AgentEvent, ModelCallId, OperationId, OperationStatus, SettlementDurability, TokenUsage,
        ToolBatchId, ToolCallId, TurnId,
    };
    use philo_tools::{ToolDisplay, ToolResult};

    fn apply_all(events: &[AgentEvent], level: InfoLevel, show_reasoning: bool) -> TranscriptStore {
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

    fn event_sequence() -> Vec<AgentEvent> {
        vec![
            AgentEvent::OperationStarted {
                operation_id: OperationId::new("op-1"),
            },
            AgentEvent::PriorTurnSealed {
                turn_id: TurnId::new("old-turn"),
            },
            AgentEvent::TurnStarted {
                turn_id: TurnId::new("turn-1"),
            },
            AgentEvent::ModelCallStarted {
                model_call_id: ModelCallId::new("call-1"),
            },
            AgentEvent::ReasoningDelta {
                model_call_id: ModelCallId::new("call-1"),
                text: "checking workspace\n".to_owned(),
            },
            AgentEvent::TextDelta {
                delta: "answer line\nfinal".to_owned(),
            },
            AgentEvent::ModelUsageUpdated {
                model_call_id: ModelCallId::new("call-1"),
                usage: TokenUsage {
                    input_tokens: Some(12),
                    output_tokens: Some(7),
                    ..TokenUsage::default()
                },
            },
            AgentEvent::ToolBatchRequested {
                tool_batch_id: ToolBatchId::new("batch-1"),
                call_count: 1,
            },
            AgentEvent::ToolExecutionStarted {
                tool_batch_id: ToolBatchId::new("batch-1"),
                tool_call_id: ToolCallId::new("tool-1"),
                index: 0,
                tool_name: "read_file".to_owned(),
                arguments: "{\"path\":\"src/main.rs\"}".to_owned(),
            },
            AgentEvent::ToolExecutionCompleted {
                tool_batch_id: ToolBatchId::new("batch-1"),
                tool_call_id: ToolCallId::new("tool-1"),
                index: 0,
                tool_name: "read_file".to_owned(),
                result: ToolResult::success("fn main() {}"),
                display: Some(ToolDisplay::new("read 12 bytes").with_fact("bytes", "12")),
            },
            AgentEvent::CancellationRequested {
                operation_id: OperationId::new("op-1"),
                reason: CancelReason::Timeout,
            },
            AgentEvent::TurnCancelled {
                turn_id: TurnId::new("turn-1"),
                reason: CancelReason::Timeout,
            },
            AgentEvent::OperationSettled {
                operation_id: OperationId::new("op-1"),
                status: OperationStatus::Cancelled,
                durability: SettlementDurability::Confirmed,
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
            &AgentEvent::ReasoningDelta {
                model_call_id: ModelCallId::new("call-1"),
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
            &AgentEvent::ReasoningDelta {
                model_call_id: ModelCallId::new("call-1"),
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
            &AgentEvent::ReasoningDelta {
                model_call_id: ModelCallId::new("call-1"),
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
        let started = AgentEvent::ToolExecutionStarted {
            tool_batch_id: ToolBatchId::new("batch"),
            tool_call_id: ToolCallId::new("tool"),
            index: 0,
            tool_name: "读取文件".repeat(40),
            arguments: "{\"path\":\"src/main.rs\"}".to_owned(),
        };
        transcript.apply(&mut store, &started, InfoLevel::Default);
        assert!(
            store.is_empty(),
            "started belongs only to Activity in default mode"
        );

        let completed = AgentEvent::ToolExecutionCompleted {
            tool_batch_id: ToolBatchId::new("batch"),
            tool_call_id: ToolCallId::new("tool"),
            index: 0,
            tool_name: "读取文件".repeat(40),
            result: ToolResult::success("内容".repeat(100)),
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
        let progress = AgentEvent::ToolExecutionProgress {
            tool_batch_id: ToolBatchId::new("batch"),
            tool_call_id: ToolCallId::new("tool"),
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
            AgentEvent::TextDelta {
                delta: "I'll look".to_owned(),
            },
            AgentEvent::ToolBatchRequested {
                tool_batch_id: ToolBatchId::new("batch"),
                call_count: 1,
            },
            AgentEvent::ToolExecutionCompleted {
                tool_batch_id: ToolBatchId::new("batch"),
                tool_call_id: ToolCallId::new("tool"),
                index: 0,
                tool_name: "read".to_owned(),
                result: ToolResult::success("ok"),
                display: None,
            },
            AgentEvent::ModelCallStarted {
                model_call_id: ModelCallId::new("call-2"),
            },
            AgentEvent::TextDelta {
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
