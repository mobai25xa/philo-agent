//! Terminal settlement paths of a turn whose start already persisted.

use super::EngineContext;
use super::invariant_failure;
use crate::mapping::entries::failure_entries;
use crate::mapping::failure::{commit_failure, describe_session_error, kernel_failure};
use crate::mapping::tool::session_result;
use crate::operation::OperationPublisher;
use crate::{AgentEvent, AgentFailure, RetryDisposition, TurnId};
use philo_agent_kernel as kernel;
use philo_session as session;
use philo_tools::RichToolResult;

/// One started turn's driving context: the engine dependencies plus the
/// mutable kernel/session cursor. Settlement methods consume the context,
/// so a settled turn cannot be driven further by construction.
pub(super) struct TurnCx<'a> {
    pub(super) ctx: &'a EngineContext,
    pub(super) operation: OperationPublisher,
    pub(super) session_id: session::SessionId,
    pub(super) revision: session::SessionRevision,
    pub(super) current_leaf: session::EntryId,
    pub(super) state: kernel::KernelState,
}

impl TurnCx<'_> {
    /// Commits the cancellation of a turn whose start already persisted:
    /// `completion_marks` (batch completion, possibly empty) plus the two
    /// terminal entries in one atomic transaction. On commit failure the
    /// `Terminated(Cancelled)` decision is discarded and the persistence
    /// error takes the established failure path over the old outstanding
    /// effect — the operation must never report `Cancelled` without the
    /// durable fact.
    pub(super) async fn cancel(
        self,
        effect_id: kernel::EffectId,
        completion_marks: Vec<session::SessionEntryKind>,
        executed_events: Vec<AgentEvent>,
    ) {
        if kernel::transition(
            &self.state,
            kernel::KernelInput::CancelRequested {
                effect_id: effect_id.clone(),
            },
        )
        .is_err()
        {
            self.fail(effect_id, invariant_failure("kernel rejected CancelRequested"))
                .await;
            return;
        }
        // The accepted reason (user request or operation timeout) becomes
        // part of the durable terminal facts; the kernel never sees it.
        let reason = self.operation.cancel_reason();
        let usage = self.operation.shared().last_usage();
        let generation = crate::mapping::session_generation_choice(self.ctx.generation.as_ref());
        let mut entries = completion_marks;
        entries.push(session::SessionEntryKind::TurnTerminated {
            turn_id: session::TurnId::new(self.operation.turn_id().as_str()),
            outcome: session::TurnOutcome::Cancelled { reason },
        });
        entries.push(session::SessionEntryKind::OperationSettled {
            operation_id: session::OperationId::new(self.operation.operation_id().as_str()),
            outcome: session::OperationOutcome::Cancelled { reason },
            usage: usage.map(crate::mapping::session_usage),
            generation: Some(generation),
        });
        let commit = self
            .ctx
            .sessions
            .commit(session::SessionTransaction::linear(
                self.session_id.clone(),
                self.revision,
                entries,
            ))
            .await;
        match commit {
            Ok(commit) => {
                for event in executed_events {
                    self.operation.push(event).await;
                }
                self.operation.cancel_committed(commit.revision()).await;
            }
            Err(error) => {
                self.fail(
                    effect_id,
                    commit_failure(
                        "engine.cancel_commit_failed",
                        RetryDisposition::MayDuplicate { retry_after_ms: None },
                        "committing cancellation",
                        &error,
                    ),
                )
                .await;
            }
        }
    }

    /// Requests kernel termination for `failure` and commits the failure
    /// settlement; a rejected transition or failed commit degrades to an
    /// unconfirmed settlement.
    pub(super) async fn fail(self, effect_id: kernel::EffectId, failure: AgentFailure) {
        let usage = self.operation.shared().last_usage();
        let termination = match kernel::transition(
            &self.state,
            kernel::KernelInput::TerminationRequested {
                effect_id,
                failure: kernel_failure(&failure),
            },
        ) {
            Ok(value) => value,
            Err(_) => {
                self.operation.fail_unconfirmed(failure).await;
                return;
            }
        };
        let generation = crate::mapping::session_generation_choice(self.ctx.generation.as_ref());
        let entries = match failure_entries(
            &termination.observations,
            self.operation.operation_id(),
            self.operation.turn_id(),
            usage,
            &generation,
        ) {
            Ok(value) => value,
            Err(_) => {
                self.operation.fail_unconfirmed(failure).await;
                return;
            }
        };
        let commit = self
            .ctx
            .sessions
            .commit(session::SessionTransaction::linear(
                self.session_id.clone(),
                self.revision,
                entries,
            ))
            .await;
        match commit {
            Ok(commit) => {
                self.operation
                    .fail_confirmed(failure, commit.revision())
                    .await
            }
            Err(error) => {
                self.operation
                    .fail_unconfirmed(failure.with_appended_diagnostic(&describe_session_error(
                        &error,
                    )))
                    .await
            }
        }
    }
}

/// One call's end inside a cancelled batch: complete result, started-but-
/// stopped, or never invoked.
pub(super) enum BatchSlot {
    Done(RichToolResult),
    Stopped,
    Unstarted,
}

/// Builds completion marks and post-commit events for a mid-batch
/// cancellation. Every call gets one durable slot in source order.
/// Only [`BatchSlot::Done`] publishes `ToolExecutionCompleted`.
pub(super) fn cancellation_batch(
    turn_id: &TurnId,
    batch_id: &kernel::ToolBatchId,
    calls: &[kernel::KernelToolCall],
    slots: &[BatchSlot],
) -> (Vec<session::SessionEntryKind>, Vec<AgentEvent>) {
    let turn = session::TurnId::new(turn_id.as_str());
    let batch = session::ToolBatchId::new(batch_id.as_str());
    let mut marks = Vec::new();
    let mut executed_events = Vec::new();
    for (index, (call, slot)) in calls.iter().zip(slots.iter()).enumerate() {
        match slot {
            BatchSlot::Done(rich) => {
                marks.push(session::SessionEntryKind::ToolResult {
                    turn_id: turn.clone(),
                    tool_batch_id: batch.clone(),
                    result: session_result(call, rich.result()),
                });
                executed_events.push(AgentEvent::ToolExecutionCompleted {
                    tool_batch_id: crate::ToolBatchId::new(batch_id.as_str()),
                    tool_call_id: crate::ToolCallId::new(call.id().as_str()),
                    index,
                    tool_name: call.name().to_owned(),
                    result: rich.result().clone(),
                    display: rich.display().cloned(),
                });
            }
            BatchSlot::Stopped => marks.push(session::SessionEntryKind::ToolResult {
                turn_id: turn.clone(),
                tool_batch_id: batch.clone(),
                result: session::SessionToolResult::interrupted(session::ToolCallId::new(
                    call.id().as_str(),
                )),
            }),
            BatchSlot::Unstarted => marks.push(session::SessionEntryKind::ToolResult {
                turn_id: turn.clone(),
                tool_batch_id: batch.clone(),
                result: session::SessionToolResult::cancelled(session::ToolCallId::new(
                    call.id().as_str(),
                )),
            }),
        }
    }
    (marks, executed_events)
}
