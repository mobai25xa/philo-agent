//! Ephemeral projection of the operation currently occupying the agent.

use philo_agent_service::FrontendOperationEvent;

use super::text;
use super::transcript::compact_args;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActivityTone {
    Normal,
    Reasoning,
    Tool,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActivityView {
    pub(crate) text: String,
    pub(crate) tone: ActivityTone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ActivityKind {
    Waiting(&'static str),
    Responding,
    Reasoning,
    Tool {
        running: Vec<RunningTool>,
        total: usize,
    },
    Compacting,
    Cancelling(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RunningTool {
    name: String,
    arguments: String,
    index: usize,
    tail: String,
}

impl ActivityKind {
    fn priority(&self) -> u8 {
        match self {
            Self::Waiting(_) | Self::Responding => 1,
            Self::Reasoning => 2,
            Self::Tool { .. } => 3,
            Self::Compacting => 4,
            Self::Cancelling(_) => 5,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ActivityState {
    current: Option<ActivityKind>,
    spinner: usize,
    tool_batch_size: usize,
}

impl ActivityState {
    pub(crate) fn is_active(&self) -> bool {
        self.current.is_some()
    }

    pub(crate) fn clear(&mut self) {
        self.current = None;
        self.spinner = 0;
        self.tool_batch_size = 0;
    }

    pub(crate) fn wait_for_model(&mut self) {
        self.set(ActivityKind::Waiting("wait"));
    }

    /// One live-timeline row for the current tool batch.
    pub(crate) fn timeline_row(&self, width: usize) -> Option<String> {
        let Some(ActivityKind::Tool { running, .. }) = &self.current else {
            return None;
        };
        let row = match running.as_slice() {
            [tool] => {
                let args = compact_args(&tool.arguments);
                let last = last_live_line(&tool.tail);
                if !last.is_empty() {
                    format!("{}  {last}", tool.name)
                } else if args.is_empty() {
                    tool.name.clone()
                } else {
                    format!("{}  {args}", tool.name)
                }
            }
            _ => running
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        };
        Some(text::truncate(&row, width))
    }

    /// Extra rows for the live band: tool arguments and parallel call names.
    pub(crate) fn detail_rows(
        &self,
        width: usize,
        height: usize,
        tail_lines: usize,
    ) -> Vec<String> {
        if height == 0 || width == 0 {
            return Vec::new();
        }
        let Some(ActivityKind::Tool { running, .. }) = &self.current else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for tool in running {
            let args = compact_args(&tool.arguments);
            if running.len() == 1 {
                rows.push(text::truncate(&args, width));
            } else {
                rows.push(text::truncate(
                    &format!("{}  {}  {args}", tool.index + 1, tool.name),
                    width,
                ));
            }
            if rows.len() >= height {
                break;
            }
            if tail_lines > 0 && !tool.tail.is_empty() {
                let remaining = height - rows.len();
                let take = tail_lines.min(remaining);
                rows.extend(text::tail_rows(&tool.tail, width, take));
            }
            if rows.len() >= height {
                break;
            }
        }
        rows
    }

    pub(crate) fn start_manual_compaction(&mut self) {
        self.replace(ActivityKind::Compacting);
    }

    pub(crate) fn finish_manual_compaction(&mut self) {
        if matches!(self.current, Some(ActivityKind::Compacting)) {
            self.clear();
        }
    }

    pub(crate) fn advance_spinner(&mut self) {
        if self.current.is_some() {
            self.spinner = (self.spinner + 1) % 4;
        }
    }

    pub(crate) fn view(&self, max_width: usize) -> Option<ActivityView> {
        let kind = self.current.as_ref()?;
        let (label, tone) = match kind {
            ActivityKind::Waiting(label) => ((*label).to_owned(), ActivityTone::Normal),
            ActivityKind::Responding => ("write".to_owned(), ActivityTone::Normal),
            ActivityKind::Reasoning => ("think".to_owned(), ActivityTone::Reasoning),
            ActivityKind::Tool { running, total } => {
                let total = (*total).max(running.last().map_or(0, |tool| tool.index + 1));
                let label = match running.as_slice() {
                    [tool] => format!("tool  {}", tool.name),
                    _ => format!("tools  {}/{total}", running.len()),
                };
                (label, ActivityTone::Tool)
            }
            ActivityKind::Compacting => ("compact".to_owned(), ActivityTone::Normal),
            ActivityKind::Cancelling(reason) => (
                format!("cancel  {}", reason_text(reason)),
                ActivityTone::Warning,
            ),
        };
        let spinner = ["|", "/", "-", "\\"][self.spinner];
        Some(ActivityView {
            text: text::truncate(&format!("{spinner} {label}"), max_width),
            tone,
        })
    }

    pub(crate) fn on_event(&mut self, event: &FrontendOperationEvent) {
        match event {
            FrontendOperationEvent::OperationQueued { .. } => {
                self.set(ActivityKind::Waiting("wait"));
            }
            FrontendOperationEvent::OperationStarted { .. }
            | FrontendOperationEvent::TurnStarted { .. }
            | FrontendOperationEvent::ModelCallStarted { .. } => self.wait_for_model(),
            FrontendOperationEvent::ModelResponseStarted { .. } => {
                self.set(ActivityKind::Waiting("wait"));
            }
            FrontendOperationEvent::ReasoningDelta { .. } => self.set(ActivityKind::Reasoning),
            FrontendOperationEvent::TextDelta { .. } => self.set(ActivityKind::Responding),
            FrontendOperationEvent::ToolBatchRequested { call_count, .. } => {
                self.tool_batch_size = *call_count;
                self.set(ActivityKind::Waiting("wait"));
            }
            FrontendOperationEvent::ToolExecutionStarted {
                tool_name,
                arguments,
                index,
                ..
            } => {
                let mut running = match &self.current {
                    Some(ActivityKind::Tool { running, .. }) => running.clone(),
                    _ => Vec::new(),
                };
                if let Some(existing) = running.iter_mut().find(|tool| tool.index == *index) {
                    existing.name = tool_name.clone();
                    existing.arguments = arguments.clone();
                } else {
                    running.push(RunningTool {
                        name: tool_name.clone(),
                        arguments: arguments.clone(),
                        index: *index,
                        tail: String::new(),
                    });
                    running.sort_by_key(|tool| tool.index);
                }
                self.set(ActivityKind::Tool {
                    running,
                    total: self.tool_batch_size,
                });
            }
            FrontendOperationEvent::ToolExecutionProgress { index, tail, .. } => {
                if matches!(self.current, Some(ActivityKind::Cancelling(_))) {
                    return;
                }
                if let Some(ActivityKind::Tool { running, .. }) = &mut self.current
                    && let Some(tool) = running.iter_mut().find(|tool| tool.index == *index)
                {
                    tool.tail = sanitize_live_tail(tail);
                }
            }
            FrontendOperationEvent::ToolExecutionCompleted { index, .. } => {
                let next = match &self.current {
                    Some(ActivityKind::Tool { running, total }) => {
                        let running: Vec<_> = running
                            .iter()
                            .filter(|tool| tool.index != *index)
                            .cloned()
                            .collect();
                        if running.is_empty() {
                            Some(ActivityKind::Waiting("wait"))
                        } else {
                            Some(ActivityKind::Tool {
                                running,
                                total: *total,
                            })
                        }
                    }
                    _ => None,
                };
                if let Some(next) = next {
                    self.replace(next);
                }
            }
            FrontendOperationEvent::ContextCompactionCompleted { .. }
            | FrontendOperationEvent::ContextCompactionFailed { .. } => {
                if matches!(self.current, Some(ActivityKind::Compacting)) {
                    self.replace(ActivityKind::Waiting("wait"));
                }
            }
            FrontendOperationEvent::ContextCompactionStarted => self.set(ActivityKind::Compacting),
            FrontendOperationEvent::CancellationRequested { reason, .. }
            | FrontendOperationEvent::TurnCancelled { reason, .. } => {
                self.replace(ActivityKind::Cancelling(reason.clone()));
            }
            FrontendOperationEvent::AssistantMessageCompleted { .. } => {
                self.set(ActivityKind::Waiting("wait"));
            }
            FrontendOperationEvent::TurnFailed { .. }
            | FrontendOperationEvent::OperationSettled { .. } => self.clear(),
            _ => {}
        }
    }

    fn set(&mut self, next: ActivityKind) {
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.priority() > next.priority())
        {
            return;
        }
        self.replace(next);
    }

    fn replace(&mut self, next: ActivityKind) {
        self.current = Some(next);
    }
}

fn last_live_line(tail: &str) -> &str {
    tail.lines().next_back().unwrap_or("").trim()
}

fn sanitize_live_tail(raw: &str) -> String {
    strip_controls(&apply_carriage_returns(&strip_ansi(raw)))
}

fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1B {
            index += 1;
            if index < bytes.len() && bytes[index] == b'[' {
                index += 1;
                while index < bytes.len() && !bytes[index].is_ascii_alphabetic() {
                    index += 1;
                }
                if index < bytes.len() {
                    index += 1;
                }
                continue;
            }
            continue;
        }
        let next = input[index..].chars().next().expect("valid utf-8");
        out.push(next);
        index += next.len_utf8();
    }
    out
}

fn apply_carriage_returns(input: &str) -> String {
    input
        .split('\n')
        .map(|line| line.rsplit('\r').next().unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_controls(input: &str) -> String {
    input
        .chars()
        .filter(|ch| *ch == '\n' || *ch == '\t' || !ch.is_control())
        .collect()
}

fn reason_text(reason: &str) -> String {
    match reason {
        "User" | "user" => "user".to_owned(),
        "Timeout" | "timeout" => "timeout".to_owned(),
        "Abandoned" | "abandoned" => "abandoned".to_owned(),
        other => other.to_ascii_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use philo_agent_service::{FrontendOperationEvent, FrontendToolResult};

    use super::*;

    fn completed(index: usize, name: &str, content: &str) -> FrontendOperationEvent {
        FrontendOperationEvent::ToolExecutionCompleted {
            tool_batch_id: "batch".to_owned(),
            tool_call_id: format!("call-{index}"),
            index,
            tool_name: name.to_owned(),
            result: FrontendToolResult::Success {
                content: content.to_owned(),
            },
            display: None,
        }
    }

    #[test]
    fn tool_activity_is_ephemeral_and_terminal_events_clean_it_up() {
        let mut state = ActivityState::default();
        state.on_event(&FrontendOperationEvent::ToolBatchRequested {
            tool_batch_id: "batch".to_owned(),
            call_count: 2,
        });
        state.on_event(&FrontendOperationEvent::ToolExecutionStarted {
            tool_batch_id: "batch".to_owned(),
            tool_call_id: "call".to_owned(),
            index: 0,
            tool_name: "read_file".to_owned(),
            arguments: "{\"path\":\"src/很长的文件.rs\"}".to_owned(),
        });
        let view = state.view(36).expect("activity");
        assert!(view.text.contains("tool  read_file"), "{view:?}");
        assert!(text::width(&view.text) <= 36);
        assert_eq!(state.detail_rows(40, 2, 5), ["path: src/很长的文件.rs"]);

        state.on_event(&completed(0, "read_file", "ok"));
        assert!(state.view(80).expect("waiting").text.contains("wait"));

        state.on_event(&FrontendOperationEvent::OperationSettled {
            operation_id: "op".to_owned(),
            session_id: "s-1".to_owned(),
            status: "Succeeded".to_owned(),
            durability: "Confirmed".to_owned(),
            session_revision: philo_agent_service::SettlementRevision::Unchanged,
        });
        assert!(!state.is_active());
    }

    #[test]
    fn concurrent_tool_starts_stay_visible_together() {
        let mut state = ActivityState::default();
        state.on_event(&FrontendOperationEvent::ToolBatchRequested {
            tool_batch_id: "batch".to_owned(),
            call_count: 3,
        });
        state.on_event(&FrontendOperationEvent::ToolExecutionStarted {
            tool_batch_id: "batch".to_owned(),
            tool_call_id: "call-1".to_owned(),
            index: 0,
            tool_name: "read_file".to_owned(),
            arguments: "{\"path\":\"a.rs\"}".to_owned(),
        });
        state.on_event(&FrontendOperationEvent::ToolExecutionStarted {
            tool_batch_id: "batch".to_owned(),
            tool_call_id: "call-2".to_owned(),
            index: 1,
            tool_name: "grep".to_owned(),
            arguments: "{\"pattern\":\"fn\"}".to_owned(),
        });
        let view = state.view(80).expect("parallel activity");
        assert!(view.text.contains("tools  2/3"), "{view:?}");

        state.on_event(&completed(0, "read_file", "ok"));
        let remaining = state.view(80).expect("one still running");
        assert!(remaining.text.contains("tool  grep"), "{remaining:?}");
    }

    #[test]
    fn cancellation_cannot_be_overwritten_by_late_text() {
        let mut state = ActivityState::default();
        state.on_event(&FrontendOperationEvent::CancellationRequested {
            operation_id: "op".to_owned(),
            reason: "User".to_owned(),
        });
        state.on_event(&FrontendOperationEvent::TextDelta {
            delta: "late".to_owned(),
        });
        state.on_event(&completed(0, "late_tool", "late"));
        assert!(state.view(80).expect("activity").text.contains("cancel"));
    }

    #[test]
    fn progress_updates_the_live_tail_and_cannot_outrank_cancel() {
        let mut state = ActivityState::default();
        state.on_event(&FrontendOperationEvent::ToolBatchRequested {
            tool_batch_id: "batch".to_owned(),
            call_count: 1,
        });
        state.on_event(&FrontendOperationEvent::ToolExecutionStarted {
            tool_batch_id: "batch".to_owned(),
            tool_call_id: "call".to_owned(),
            index: 0,
            tool_name: "shell".to_owned(),
            arguments: "{\"command\":\"echo hi\"}".to_owned(),
        });
        state.on_event(&FrontendOperationEvent::ToolExecutionProgress {
            tool_batch_id: "batch".to_owned(),
            tool_call_id: "call".to_owned(),
            index: 0,
            tail: "\u{1b}[32mhello\u{1b}[0m\rworld".to_owned(),
        });
        let details = state.detail_rows(40, 4, 5);
        assert!(
            details.iter().any(|row| row.contains("world")),
            "{details:?}"
        );
        assert!(
            details
                .iter()
                .all(|row| !row.contains("hello") && !row.contains("\u{1b}")),
            "{details:?}"
        );

        state.on_event(&FrontendOperationEvent::CancellationRequested {
            operation_id: "op".to_owned(),
            reason: "User".to_owned(),
        });
        state.on_event(&FrontendOperationEvent::ToolExecutionProgress {
            tool_batch_id: "batch".to_owned(),
            tool_call_id: "call".to_owned(),
            index: 0,
            tail: "late-progress".to_owned(),
        });
        assert!(state.view(80).expect("activity").text.contains("cancel"));
    }
}
