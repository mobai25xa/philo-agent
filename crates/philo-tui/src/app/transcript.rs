//! Pure event-to-line projection for the scrollback transcript.
//!
//! History lines are append-only once produced (inline discipline: written
//! lines are never rewritten). Streaming text accumulates in a partial line
//! exposed separately for the live row of the bottom panel.

use philo_agent_runtime::{AgentEvent, CancelReason, OperationStatus, SettlementDurability};

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

fn line(kind: LineKind, text: impl Into<String>) -> TranscriptLine {
    TranscriptLine {
        kind,
        text: text.into(),
    }
}

/// Streaming projection state for one operation's events.
#[derive(Debug, Default)]
pub struct Transcript {
    partial_answer: String,
    partial_reasoning: String,
    tool_batch_size: usize,
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

    /// The unfinished streaming line for the live row, if any.
    pub fn partial(&self) -> Option<(LineKind, &str)> {
        if !self.partial_answer.is_empty() {
            return Some((LineKind::Answer, self.partial_answer.as_str()));
        }
        if !self.partial_reasoning.is_empty() {
            return Some((LineKind::Reasoning, self.partial_reasoning.as_str()));
        }
        None
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

    fn flush_reasoning(&mut self, lines: &mut Vec<TranscriptLine>) {
        if !self.partial_reasoning.is_empty() {
            let text = std::mem::take(&mut self.partial_reasoning);
            lines.push(line(LineKind::Reasoning, format!("[reasoning] {text}")));
        }
    }

    /// Projects one event into completed transcript lines.
    #[allow(clippy::too_many_lines)]
    pub fn on_event(&mut self, event: &AgentEvent, level: InfoLevel) -> Vec<TranscriptLine> {
        let verbose = level == InfoLevel::Verbose;
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
                while let Some(newline) = self.partial_reasoning.find('\n') {
                    let mut completed: String = self.partial_reasoning.drain(..=newline).collect();
                    completed.pop();
                    if !completed.is_empty() {
                        lines.push(line(
                            LineKind::Reasoning,
                            format!("[reasoning] {completed}"),
                        ));
                    }
                }
            }
            AgentEvent::OperationQueued { .. } => {
                lines.push(line(
                    LineKind::Notice,
                    "queued: waiting for the active turn",
                ));
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
            // Usage drives the status bar, not the transcript.
            AgentEvent::ModelUsageUpdated { .. } => {}
            AgentEvent::ToolBatchRequested {
                tool_batch_id,
                call_count,
            } => {
                self.flush_reasoning(&mut lines);
                self.tool_batch_size = *call_count;
                if verbose {
                    lines.push(line(
                        LineKind::Tool,
                        format!("tool batch {tool_batch_id}: {call_count} call(s)"),
                    ));
                }
            }
            AgentEvent::ToolExecutionStarted {
                tool_name,
                arguments,
                index,
                ..
            } => {
                self.flush_reasoning(&mut lines);
                if verbose {
                    lines.push(line(
                        LineKind::Tool,
                        format!(
                            "tool {}/{} {tool_name} arguments: {arguments}",
                            index + 1,
                            self.tool_batch_size,
                        ),
                    ));
                } else {
                    lines.push(line(
                        LineKind::Tool,
                        format!("tool {tool_name}({})", preview(arguments, 60)),
                    ));
                }
            }
            AgentEvent::ToolExecutionCompleted {
                tool_name,
                result,
                display,
                ..
            } => {
                self.flush_reasoning(&mut lines);
                if verbose {
                    let full = match result {
                        philo_agent_runtime::ToolResult::Success { content } => content.clone(),
                        philo_agent_runtime::ToolResult::Error { code, message } => {
                            format!("[{code}] {message}")
                        }
                    };
                    lines.push(line(LineKind::Tool, format!("tool {tool_name} result:")));
                    lines.extend(
                        full.lines()
                            .map(|result_line| line(LineKind::Tool, format!("  {result_line}"))),
                    );
                    if let Some(display) = display {
                        if !display.detail().is_empty() {
                            lines.push(line(LineKind::Tool, "detail:"));
                            lines.extend(display.detail().lines().map(|detail_line| {
                                line(LineKind::Tool, format!("  {detail_line}"))
                            }));
                        }
                        if !display.facts().is_empty() {
                            let facts = display
                                .facts()
                                .iter()
                                .map(|fact| format!("{}={}", fact.name(), fact.value()))
                                .collect::<Vec<_>>()
                                .join(" ");
                            lines.push(line(LineKind::Tool, format!("facts: {facts}")));
                        }
                    }
                } else {
                    let summary = match result {
                        philo_agent_runtime::ToolResult::Success { content } => {
                            format!("ok: {}", preview(content, 80))
                        }
                        philo_agent_runtime::ToolResult::Error { code, message } => {
                            format!("error {code}: {}", preview(message, 80))
                        }
                    };
                    lines.push(line(LineKind::Tool, format!("  {tool_name} -> {summary}")));
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
                            "notice: previous turn {turn_id} did not end cleanly and was \
                             sealed; its tool calls may have executed without recorded results"
                        ),
                    ));
                } else {
                    lines.push(line(
                        LineKind::Notice,
                        "notice: a previous turn did not end cleanly and was sealed; its tool \
                         calls may have executed without recorded results",
                    ));
                }
            }
            AgentEvent::CancellationRequested { reason, .. } => {
                lines.extend(self.flush_partial());
                lines.push(line(
                    LineKind::Notice,
                    format!("cancelling ({})...", reason_text(*reason)),
                ));
            }
            AgentEvent::TurnCancelled { reason, .. } => {
                lines.push(line(
                    LineKind::Notice,
                    format!("turn cancelled ({})", reason_text(*reason)),
                ));
            }
            AgentEvent::TurnFailed { failure, .. } => {
                lines.extend(self.flush_partial());
                lines.push(line(
                    LineKind::Error,
                    format!(
                        "error: turn failed ({:?}): {}",
                        failure.kind(),
                        failure.message(),
                    ),
                ));
            }
            AgentEvent::OperationSettled {
                status, durability, ..
            } => {
                lines.extend(self.flush_partial());
                let status_text = match status {
                    OperationStatus::Succeeded => "succeeded",
                    OperationStatus::Failed => "failed",
                    OperationStatus::Cancelled => "cancelled",
                };
                lines.push(line(LineKind::Meta, format!("done ({status_text})")));
                if *durability == SettlementDurability::Unconfirmed {
                    lines.push(line(
                        LineKind::Error,
                        "WARNING: settlement durability UNCONFIRMED - the session may not \
                         have durably recorded this outcome",
                    ));
                }
                self.tool_batch_size = 0;
            }
            // Future events: tolerate quietly (#[non_exhaustive]).
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

/// Single-line preview: newlines collapse, long text truncates on a char
/// boundary with an ellipsis.
pub(crate) fn preview(text: &str, max_chars: usize) -> String {
    let flat = text.replace(['\n', '\r'], " ");
    let flat = flat.trim();
    if flat.chars().count() <= max_chars {
        return flat.to_owned();
    }
    let kept: String = flat.chars().take(max_chars).collect();
    format!("{kept}...")
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
