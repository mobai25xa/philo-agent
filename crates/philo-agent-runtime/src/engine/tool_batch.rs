//! One committed tool batch: sequential execution and the results barrier.

use super::settlement::{TurnCx, cancellation_batch};
use crate::mapping::failure::session_failure;
use crate::mapping::tool::session_result;
use crate::{AgentEvent, AgentFailure, AgentFailureKind, OperationPhase, RunningToolBatchPhase};
use philo_agent_kernel as kernel;
use philo_session as session;
use philo_tools::{RichToolResult, ToolInvocation};

/// Executes and commits one tool batch. Returns the advanced context and
/// real results; `None` when a failure or cancellation already settled the
/// operation.
pub(super) async fn run<'a>(
    mut cx: TurnCx<'a>,
    effect_id: &kernel::EffectId,
    batch_id: &kernel::ToolBatchId,
    calls: &[kernel::KernelToolCall],
) -> Option<(TurnCx<'a>, Vec<(kernel::KernelToolCall, RichToolResult)>)> {
    let mut results: Vec<(kernel::KernelToolCall, RichToolResult)> = Vec::new();
    for (index, call) in calls.iter().enumerate() {
        // Injection point: barrier between tool calls (and before the
        // first). The executing call always runs to completion; later
        // calls never start.
        if cx.operation.is_cancel_requested() {
            cx.operation.cancellation_observed();
            let (marks, executed_events) =
                cancellation_batch(cx.operation.turn_id(), batch_id, calls, &results);
            cx.cancel(effect_id.clone(), marks, executed_events).await;
            return None;
        }
        cx.operation.set_phase(OperationPhase::RunningToolBatch(
            RunningToolBatchPhase::Executing { index },
        ));
        cx.operation.push(AgentEvent::ToolExecutionStarted {
            tool_batch_id: crate::ToolBatchId::new(batch_id.as_str()),
            tool_call_id: crate::ToolCallId::new(call.id().as_str()),
            index,
            tool_name: call.name().to_owned(),
            arguments: call.arguments().to_owned(),
        });
        let invoked = cx
            .ctx
            .tools
            .invoke(ToolInvocation::new(
                call.id().as_str(),
                call.name(),
                call.arguments(),
            ))
            .await;
        let result = match invoked {
            Ok(result) => result,
            Err(error) => {
                cx.fail(
                    effect_id.clone(),
                    AgentFailure::new(AgentFailureKind::ToolExecution, error.message()),
                )
                .await;
                return None;
            }
        };
        results.push((call.clone(), result));
    }
    // Injection point: after the last call finished, before the results
    // barrier commits: all-real completion, empty suffix.
    if cx.operation.is_cancel_requested() {
        cx.operation.cancellation_observed();
        let (marks, executed_events) =
            cancellation_batch(cx.operation.turn_id(), batch_id, calls, &results);
        cx.cancel(effect_id.clone(), marks, executed_events).await;
        return None;
    }
    cx.operation.set_phase(OperationPhase::RunningToolBatch(
        RunningToolBatchPhase::CommittingResults,
    ));
    // Model channel only: display never enters the Session.
    let result_entries = results
        .iter()
        .map(|(call, rich)| session::SessionEntryKind::ToolResult {
            turn_id: session::TurnId::new(cx.operation.turn_id().as_str()),
            tool_batch_id: session::ToolBatchId::new(batch_id.as_str()),
            result: session_result(call, rich.result()),
        })
        .collect();
    let committed = cx
        .ctx
        .sessions
        .commit(session::SessionTransaction::linear(
            cx.session_id.clone(),
            cx.revision,
            result_entries,
        ))
        .await;
    let results_commit = match committed {
        Ok(commit) => commit,
        Err(error) => {
            cx.fail(
                effect_id.clone(),
                session_failure("committing tool results", &error),
            )
            .await;
            return None;
        }
    };
    for (index, (call, rich)) in results.iter().enumerate() {
        cx.operation.push(AgentEvent::ToolExecutionCompleted {
            tool_batch_id: crate::ToolBatchId::new(batch_id.as_str()),
            tool_call_id: crate::ToolCallId::new(call.id().as_str()),
            index,
            tool_name: call.name().to_owned(),
            result: rich.result().clone(),
            display: rich.display().cloned(),
        });
    }
    cx.revision = results_commit.revision();
    cx.current_leaf = results_commit.current_leaf().clone();
    Some((cx, results))
}
