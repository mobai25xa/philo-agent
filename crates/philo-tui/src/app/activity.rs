//! Ephemeral projection of the operation currently occupying the agent.

use philo_agent_runtime::{AgentEvent, CancelReason};

use super::text;
use super::transcript::preview;

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
        name: String,
        arguments: String,
        index: usize,
        total: usize,
    },
    Compacting,
    Cancelling(CancelReason),
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
        self.set(ActivityKind::Waiting("Waiting for model"));
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
            ActivityKind::Responding => ("Writing response".to_owned(), ActivityTone::Normal),
            ActivityKind::Reasoning => ("Reasoning".to_owned(), ActivityTone::Reasoning),
            ActivityKind::Tool {
                name,
                arguments,
                index,
                total,
            } => (
                format!(
                    "Tool {}/{}: {name} {}",
                    index + 1,
                    (*total).max(index + 1),
                    preview(arguments, max_width.saturating_sub(20))
                ),
                ActivityTone::Tool,
            ),
            ActivityKind::Compacting => ("Compacting context".to_owned(), ActivityTone::Normal),
            ActivityKind::Cancelling(reason) => (
                format!("Cancelling ({})", reason_text(*reason)),
                ActivityTone::Warning,
            ),
        };
        let spinner = ["|", "/", "-", "\\"][self.spinner];
        Some(ActivityView {
            text: text::truncate(&format!("{spinner} {label}"), max_width),
            tone,
        })
    }

    pub(crate) fn on_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::OperationQueued { .. } => {
                self.set(ActivityKind::Waiting("Queued behind active turn"));
            }
            AgentEvent::OperationStarted { .. }
            | AgentEvent::TurnStarted { .. }
            | AgentEvent::ModelCallStarted { .. } => self.wait_for_model(),
            AgentEvent::ModelResponseStarted { .. } => {
                self.set(ActivityKind::Waiting("Receiving model response"));
            }
            AgentEvent::ReasoningDelta { .. } => self.set(ActivityKind::Reasoning),
            AgentEvent::TextDelta { .. } => self.set(ActivityKind::Responding),
            AgentEvent::ToolBatchRequested { call_count, .. } => {
                self.tool_batch_size = *call_count;
                self.set(ActivityKind::Waiting("Preparing tools"));
            }
            AgentEvent::ToolExecutionStarted {
                tool_name,
                arguments,
                index,
                ..
            } => self.set(ActivityKind::Tool {
                name: tool_name.clone(),
                arguments: arguments.clone(),
                index: *index,
                total: self.tool_batch_size,
            }),
            AgentEvent::ToolExecutionCompleted { .. } => {
                if matches!(self.current, Some(ActivityKind::Tool { .. })) {
                    self.replace(ActivityKind::Waiting("Waiting for model"));
                }
            }
            AgentEvent::ContextCompactionCompleted { .. }
            | AgentEvent::ContextCompactionFailed { .. } => {
                if matches!(self.current, Some(ActivityKind::Compacting)) {
                    self.replace(ActivityKind::Waiting("Waiting for model"));
                }
            }
            AgentEvent::ContextCompactionStarted => self.set(ActivityKind::Compacting),
            AgentEvent::CancellationRequested { reason, .. }
            | AgentEvent::TurnCancelled { reason, .. } => {
                self.replace(ActivityKind::Cancelling(*reason));
            }
            AgentEvent::AssistantMessageCompleted { .. } => {
                self.set(ActivityKind::Waiting("Finalizing response"));
            }
            AgentEvent::TurnFailed { .. } | AgentEvent::OperationSettled { .. } => self.clear(),
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

fn reason_text(reason: CancelReason) -> &'static str {
    match reason {
        CancelReason::User => "user",
        CancelReason::Timeout => "timeout",
        CancelReason::Abandoned => "abandoned",
    }
}

#[cfg(test)]
mod tests {
    use philo_agent_runtime::{
        OperationId, OperationStatus, SettlementDurability, ToolBatchId, ToolCallId,
    };
    use philo_tools::ToolResult;

    use super::*;

    #[test]
    fn tool_activity_is_ephemeral_and_terminal_events_clean_it_up() {
        let mut state = ActivityState::default();
        state.on_event(&AgentEvent::ToolBatchRequested {
            tool_batch_id: ToolBatchId::new("batch"),
            call_count: 2,
        });
        state.on_event(&AgentEvent::ToolExecutionStarted {
            tool_batch_id: ToolBatchId::new("batch"),
            tool_call_id: ToolCallId::new("call"),
            index: 0,
            tool_name: "read_file".to_owned(),
            arguments: "{\"path\":\"src/很长的文件.rs\"}".to_owned(),
        });
        let view = state.view(36).expect("activity");
        assert!(view.text.contains("Tool 1/2: read_file"), "{view:?}");
        assert!(text::width(&view.text) <= 36);

        state.on_event(&AgentEvent::ToolExecutionCompleted {
            tool_batch_id: ToolBatchId::new("batch"),
            tool_call_id: ToolCallId::new("call"),
            index: 0,
            tool_name: "read_file".to_owned(),
            result: ToolResult::success("ok"),
            display: None,
        });
        assert!(state.view(80).expect("waiting").text.contains("Waiting"));

        state.on_event(&AgentEvent::OperationSettled {
            operation_id: OperationId::new("op"),
            status: OperationStatus::Succeeded,
            durability: SettlementDurability::Confirmed,
        });
        assert!(!state.is_active());
    }

    #[test]
    fn cancellation_cannot_be_overwritten_by_late_text() {
        let mut state = ActivityState::default();
        state.on_event(&AgentEvent::CancellationRequested {
            operation_id: OperationId::new("op"),
            reason: CancelReason::User,
        });
        state.on_event(&AgentEvent::TextDelta {
            delta: "late".to_owned(),
        });
        state.on_event(&AgentEvent::ToolExecutionCompleted {
            tool_batch_id: ToolBatchId::new("batch"),
            tool_call_id: ToolCallId::new("call"),
            index: 0,
            tool_name: "late_tool".to_owned(),
            result: ToolResult::success("late"),
            display: None,
        });
        assert!(
            state
                .view(80)
                .expect("activity")
                .text
                .contains("Cancelling")
        );
    }
}
