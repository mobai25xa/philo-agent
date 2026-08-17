//! Pure event-to-line projection for the scrollback transcript.
//!
//! History lines are append-only once produced (inline discipline: written
//! lines are never rewritten). Streaming text accumulates in partial buffers
//! exposed separately for the live timeline of the bottom panel.

use std::collections::HashMap;

use philo_agent_runtime::{AgentEvent, CancelReason, OperationStatus, SettlementDurability};

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

/// Echoes a user message as a `You` block. Session replay uses the same
/// header for live visual grammar; tool replay stays on the older summary.
pub(crate) fn user_message_lines(text: &str) -> Vec<TranscriptLine> {
    let mut lines = vec![line(LineKind::User, "You")];
    for row in text.split('\n') {
        lines.push(line(LineKind::User, format!("  {row}")));
    }
    lines
}

/// Streaming projection state for one operation's events.
#[derive(Debug, Default)]
pub struct Transcript {
    partial_answer: String,
    partial_reasoning: String,
    think_header_written: bool,
    tool_batch_size: usize,
    tool_args: HashMap<usize, String>,
    verbose: bool,
    /// `[ui].show_reasoning`: when off, reasoning deltas are dropped rather
    /// than rendered dim (the model still receives them).
    show_reasoning: bool,
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
            self.partial_reasoning.clear();
            self.think_header_written = false;
        }
    }

    /// The unfinished streaming line for the live row, if any.
    #[cfg(test)]
    pub fn partial(&self) -> Option<(LineKind, &str)> {
        if !self.partial_answer.is_empty() {
            return Some((LineKind::Answer, self.partial_answer.as_str()));
        }
        if !self.partial_reasoning.is_empty() {
            return Some((LineKind::Reasoning, self.partial_reasoning.as_str()));
        }
        None
    }

    pub(crate) fn live_answer(&self) -> Option<&str> {
        if self.partial_answer.is_empty() {
            None
        } else {
            Some(self.partial_answer.as_str())
        }
    }

    pub(crate) fn live_reasoning(&self) -> Option<&str> {
        if self.partial_reasoning.is_empty() {
            None
        } else {
            Some(self.partial_reasoning.as_str())
        }
    }

    /// Flushes any partial content into completed lines.
    pub fn flush_partial(&mut self) -> Vec<TranscriptLine> {
        let mut lines = Vec::new();
        self.flush_reasoning(&mut lines);
        if !self.partial_answer.is_empty() {
            let text = std::mem::take(&mut self.partial_answer);
            lines.push(line(LineKind::Answer, text));
        }
        lines
    }

    fn drain_think_lines(&mut self, lines: &mut Vec<TranscriptLine>) {
        while let Some(newline) = self.partial_reasoning.find('\n') {
            let mut completed: String = self.partial_reasoning.drain(..=newline).collect();
            completed.pop();
            if !completed.is_empty() {
                self.push_think_line(lines, &completed);
            }
        }
    }

    fn push_think_line(&mut self, lines: &mut Vec<TranscriptLine>, row: &str) {
        if !self.think_header_written {
            lines.push(line(LineKind::Reasoning, "think"));
            self.think_header_written = true;
        }
        lines.push(line(LineKind::Reasoning, format!("  {row}")));
    }

    fn flush_reasoning(&mut self, lines: &mut Vec<TranscriptLine>) {
        self.drain_think_lines(lines);
        if !self.partial_reasoning.is_empty() {
            let text = std::mem::take(&mut self.partial_reasoning);
            self.push_think_line(lines, &text);
        }
        self.think_header_written = false;
    }

    fn clear_ephemeral(&mut self) {
        self.tool_batch_size = 0;
        self.tool_args.clear();
        self.think_header_written = false;
    }

    /// Projects one event into completed transcript lines.
    #[allow(clippy::too_many_lines)]
    pub fn on_event(&mut self, event: &AgentEvent, level: InfoLevel) -> Vec<TranscriptLine> {
        self.verbose = level == InfoLevel::Verbose;
        let verbose = self.verbose;
        let mut lines = Vec::new();
        match event {
            AgentEvent::TextDelta { delta } => {
                self.flush_reasoning(&mut lines);
                self.partial_answer.push_str(delta);
                while let Some(newline) = self.partial_answer.find('\n') {
                    let mut completed: String = self.partial_answer.drain(..=newline).collect();
                    completed.pop();
                    lines.push(line(LineKind::Answer, completed));
                }
            }
            AgentEvent::ReasoningDelta { .. } if !self.show_reasoning => {}
            AgentEvent::ReasoningDelta { text, .. } => {
                if !self.partial_answer.is_empty() {
                    let answer = std::mem::take(&mut self.partial_answer);
                    lines.push(line(LineKind::Answer, answer));
                }
                self.partial_reasoning.push_str(text);
                self.drain_think_lines(&mut lines);
            }
            AgentEvent::OperationQueued { .. } => {
                lines.push(line(LineKind::Notice, "queued behind the active turn"));
            }
            AgentEvent::OperationStarted { operation_id } => {
                if verbose {
                    lines.push(line(
                        LineKind::Meta,
                        format!("operation {operation_id} started"),
                    ));
                }
            }
            AgentEvent::TurnStarted { turn_id } => {
                if verbose {
                    lines.push(line(LineKind::Meta, format!("turn {turn_id} started")));
                }
            }
            AgentEvent::ModelCallStarted { model_call_id } => {
                if verbose {
                    lines.push(line(LineKind::Meta, format!("model call {model_call_id}")));
                }
            }
            AgentEvent::ModelResponseStarted {
                response_model,
                response_id,
                ..
            } => {
                if verbose {
                    lines.push(line(
                        LineKind::Meta,
                        format!(
                            "model response: model={} id={}",
                            response_model.as_deref().unwrap_or("-"),
                            response_id.as_deref().unwrap_or("-"),
                        ),
                    ));
                }
            }
            AgentEvent::ModelUsageUpdated { .. } => {}
            AgentEvent::ToolBatchRequested { call_count, .. } => {
                self.flush_reasoning(&mut lines);
                self.tool_batch_size = *call_count;
                self.tool_args.clear();
            }
            AgentEvent::ToolExecutionStarted {
                arguments, index, ..
            } => {
                self.flush_reasoning(&mut lines);
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
                self.flush_reasoning(&mut lines);
                let arguments = self.tool_args.remove(index).unwrap_or_default();
                if verbose {
                    lines.extend(tool_card::verbose_card(
                        tool_name,
                        *index,
                        self.tool_batch_size,
                        &arguments,
                        result,
                        display.as_ref(),
                    ));
                } else {
                    lines.extend(tool_card::default_card(
                        tool_name,
                        &arguments,
                        result,
                        display.as_ref(),
                    ));
                }
            }
            AgentEvent::AssistantMessageCompleted { message, .. } => {
                lines.extend(self.flush_partial());
                if lines.is_empty() && !message.content().is_empty() {
                    lines.extend(complete_text_lines(LineKind::Answer, message.content()));
                }
            }
            AgentEvent::PriorTurnSealed { turn_id } => {
                if verbose {
                    lines.push(line(
                        LineKind::Notice,
                        format!(
                            "previous turn {turn_id} did not end cleanly and was sealed; \
                             its tool calls may have executed without recorded results"
                        ),
                    ));
                } else {
                    lines.push(line(
                        LineKind::Notice,
                        "previous turn did not end cleanly and was sealed; its tool \
                         calls may have executed without recorded results",
                    ));
                }
            }
            AgentEvent::ContextCompactionStarted => {
                if verbose {
                    lines.push(line(LineKind::Notice, "compacting context..."));
                }
            }
            AgentEvent::ContextCompactionCompleted { covers_up_to } => {
                let text = if verbose {
                    format!("context compacted through {covers_up_to}")
                } else {
                    "context compacted".to_owned()
                };
                lines.push(line(LineKind::Meta, text));
            }
            AgentEvent::ContextCompactionFailed { message } => {
                lines.push(line(
                    LineKind::Error,
                    format!("compaction failed: {message}; continuing without compaction"),
                ));
            }
            AgentEvent::CancellationRequested { reason, .. } => {
                lines.extend(self.flush_partial());
                if verbose {
                    lines.push(line(
                        LineKind::Notice,
                        format!("cancelling ({})...", reason_text(*reason)),
                    ));
                }
            }
            AgentEvent::TurnCancelled { reason, .. } => {
                lines.extend(self.flush_partial());
                lines.push(line(
                    LineKind::Notice,
                    format!("turn cancelled ({})", reason_text(*reason)),
                ));
            }
            AgentEvent::TurnFailed { failure, .. } => {
                lines.extend(self.flush_partial());
                lines.push(line(
                    LineKind::Error,
                    format!("turn failed ({:?}): {}", failure.kind(), failure.message()),
                ));
            }
            AgentEvent::OperationSettled {
                status, durability, ..
            } => {
                lines.extend(self.flush_partial());
                match status {
                    OperationStatus::Succeeded => {}
                    OperationStatus::Failed => {
                        lines.push(line(LineKind::Meta, "done (failed)"));
                    }
                    OperationStatus::Cancelled => {
                        lines.push(line(LineKind::Meta, "done (cancelled)"));
                    }
                }
                if *durability == SettlementDurability::Unconfirmed {
                    lines.push(line(
                        LineKind::Error,
                        "WARNING: settlement durability UNCONFIRMED - the session may not \
                         have durably recorded this outcome",
                    ));
                }
                self.clear_ephemeral();
            }
            _ => {}
        }
        lines
    }
}

fn reason_text(reason: CancelReason) -> &'static str {
    match reason {
        CancelReason::User => "user",
        CancelReason::Timeout => "timeout",
        CancelReason::Abandoned => "abandoned",
    }
}

fn complete_text_lines(kind: LineKind, text: &str) -> Vec<TranscriptLine> {
    text.split('\n')
        .map(|text| line(kind, text.to_owned()))
        .collect()
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
mod snapshots {
    use super::*;
    use philo_agent_runtime::{
        AgentEvent, ModelCallId, OperationId, OperationStatus, SettlementDurability, TokenUsage,
        ToolBatchId, ToolCallId, TurnId,
    };
    use philo_tools::{ToolDisplay, ToolResult};

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
        let mut transcript = Transcript::new(true);
        let mut lines = Vec::new();
        for event in event_sequence() {
            lines.extend(transcript.on_event(&event, level));
        }
        lines.extend(transcript.flush_partial());
        format!(
            "lines:\n{}\npartial: {:?}",
            lines
                .iter()
                .map(|line| format!("{:?}: {}", line.kind, line.text))
                .collect::<Vec<_>>()
                .join("\n"),
            transcript.partial()
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
    fn reasoning_can_be_switched_off_entirely() {
        let mut transcript = Transcript::new(false);
        let lines = transcript.on_event(
            &AgentEvent::ReasoningDelta {
                model_call_id: ModelCallId::new("call-1"),
                text: "thinking\n".to_owned(),
            },
            InfoLevel::Verbose,
        );
        assert!(lines.is_empty(), "no reasoning reaches the transcript");
        assert_eq!(transcript.partial(), None);
    }

    #[test]
    fn default_tool_started_is_transient_and_completion_is_bounded() {
        let mut transcript = Transcript::new(true);
        let started = AgentEvent::ToolExecutionStarted {
            tool_batch_id: ToolBatchId::new("batch"),
            tool_call_id: ToolCallId::new("tool"),
            index: 0,
            tool_name: "读取文件".repeat(40),
            arguments: "{\"path\":\"src/main.rs\"}".to_owned(),
        };
        assert!(
            transcript.on_event(&started, InfoLevel::Default).is_empty(),
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
        let lines = transcript.on_event(&completed, InfoLevel::Default);
        assert_eq!(lines.len(), 1);
        assert!(text::width(&lines[0].text) <= 120);
        assert!(
            !lines[0].text.contains("内容"),
            "default cards must not dump model-facing content"
        );
    }

    #[test]
    fn tool_progress_never_writes_history() {
        let mut transcript = Transcript::new(true);
        let progress = AgentEvent::ToolExecutionProgress {
            tool_batch_id: ToolBatchId::new("batch"),
            tool_call_id: ToolCallId::new("tool"),
            index: 0,
            tail: "live output that must stay off scrollback".to_owned(),
        };
        assert!(
            transcript
                .on_event(&progress, InfoLevel::Default)
                .is_empty()
        );
        assert!(
            transcript
                .on_event(&progress, InfoLevel::Verbose)
                .is_empty()
        );
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
