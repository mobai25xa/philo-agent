//! One committed tool batch: serial or bounded-parallel execution, then the
//! results barrier.

use super::settlement::{TurnCx, cancellation_batch};
use crate::mapping::failure::session_failure;
use crate::mapping::tool::session_result;
use crate::{AgentEvent, AgentFailure, AgentFailureKind, OperationPhase, RunningToolBatchPhase};
use philo_agent_kernel as kernel;
use philo_session as session;
use philo_tools::{RichToolResult, ToolFuture, ToolInvocation, ToolPortError};
use std::task::Poll;

/// Executes and commits one tool batch. Returns the advanced context and
/// real results; `None` when a failure or cancellation already settled the
/// operation.
pub(super) async fn run<'a>(
    cx: TurnCx<'a>,
    effect_id: &kernel::EffectId,
    batch_id: &kernel::ToolBatchId,
    calls: &[kernel::KernelToolCall],
    max_parallel_tool_calls: u32,
) -> Option<(TurnCx<'a>, Vec<(kernel::KernelToolCall, RichToolResult)>)> {
    let limit = max_parallel_tool_calls.max(1);
    if limit == 1 {
        run_serial(cx, effect_id, batch_id, calls).await
    } else {
        run_parallel(cx, effect_id, batch_id, calls, limit as usize).await
    }
}

/// Existing serial path: one invoke at a time, real prefix + Cancelled suffix.
async fn run_serial<'a>(
    cx: TurnCx<'a>,
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
            RunningToolBatchPhase::Executing {
                in_flight: 1,
                completed: index,
            },
        ));
        publish_started(&cx, batch_id, call, index);
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
    commit_results(cx, effect_id, batch_id, results).await
}

/// Bounded concurrent path: start up to `limit` invokes, never start after
/// cancel or a `ToolPortError`, and await every in-flight future.
async fn run_parallel<'a>(
    cx: TurnCx<'a>,
    effect_id: &kernel::EffectId,
    batch_id: &kernel::ToolBatchId,
    calls: &[kernel::KernelToolCall],
    limit: usize,
) -> Option<(TurnCx<'a>, Vec<(kernel::KernelToolCall, RichToolResult)>)> {
    let tools = cx.ctx.tools.clone();
    let mut slots: Vec<Option<RichToolResult>> = vec![None; calls.len()];
    let mut in_flight: Vec<(usize, ToolFuture<'_>)> = Vec::new();
    let mut next = 0;
    let mut stop_starting = false;
    let mut port_error: Option<ToolPortError> = None;

    loop {
        while !stop_starting && in_flight.len() < limit && next < calls.len() {
            if cx.operation.is_cancel_requested() {
                cx.operation.cancellation_observed();
                stop_starting = true;
                break;
            }
            let index = next;
            let call = &calls[index];
            next += 1;
            set_executing(&cx, in_flight.len() + 1, completed_count(&slots));
            publish_started(&cx, batch_id, call, index);
            in_flight.push((
                index,
                tools.invoke(ToolInvocation::new(
                    call.id().as_str(),
                    call.name(),
                    call.arguments(),
                )),
            ));
        }
        if in_flight.is_empty() {
            break;
        }
        let (index, invoked) = next_finished(&mut in_flight).await;
        match invoked {
            Ok(result) => slots[index] = Some(result),
            Err(error) => {
                port_error = Some(error);
                stop_starting = true;
            }
        }
        set_executing(&cx, in_flight.len(), completed_count(&slots));
    }

    if let Some(error) = port_error {
        cx.fail(
            effect_id.clone(),
            AgentFailure::new(AgentFailureKind::ToolExecution, error.message()),
        )
        .await;
        return None;
    }

    let executed: Vec<(kernel::KernelToolCall, RichToolResult)> = calls
        .iter()
        .zip(slots.iter_mut())
        .map_while(|(call, slot)| slot.take().map(|result| (call.clone(), result)))
        .collect();
    if executed.len() < calls.len() || cx.operation.is_cancel_requested() {
        if !cx.operation.is_cancel_requested() {
            cx.operation.cancellation_observed();
        }
        let (marks, executed_events) =
            cancellation_batch(cx.operation.turn_id(), batch_id, calls, &executed);
        cx.cancel(effect_id.clone(), marks, executed_events).await;
        return None;
    }
    commit_results(cx, effect_id, batch_id, executed).await
}

async fn commit_results<'a>(
    mut cx: TurnCx<'a>,
    effect_id: &kernel::EffectId,
    batch_id: &kernel::ToolBatchId,
    results: Vec<(kernel::KernelToolCall, RichToolResult)>,
) -> Option<(TurnCx<'a>, Vec<(kernel::KernelToolCall, RichToolResult)>)> {
    // Injection point: after the last call finished, before the results
    // barrier commits: all-real completion, empty suffix.
    if cx.operation.is_cancel_requested() {
        cx.operation.cancellation_observed();
        let calls: Vec<_> = results.iter().map(|(call, _)| call.clone()).collect();
        let (marks, executed_events) =
            cancellation_batch(cx.operation.turn_id(), batch_id, &calls, &results);
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

fn publish_started(
    cx: &TurnCx<'_>,
    batch_id: &kernel::ToolBatchId,
    call: &kernel::KernelToolCall,
    index: usize,
) {
    cx.operation.push(AgentEvent::ToolExecutionStarted {
        tool_batch_id: crate::ToolBatchId::new(batch_id.as_str()),
        tool_call_id: crate::ToolCallId::new(call.id().as_str()),
        index,
        tool_name: call.name().to_owned(),
        arguments: call.arguments().to_owned(),
    });
}

fn set_executing(cx: &TurnCx<'_>, in_flight: usize, completed: usize) {
    cx.operation.set_phase(OperationPhase::RunningToolBatch(
        RunningToolBatchPhase::Executing {
            in_flight,
            completed,
        },
    ));
}

fn completed_count(slots: &[Option<RichToolResult>]) -> usize {
    slots.iter().filter(|slot| slot.is_some()).count()
}

async fn next_finished<'a>(
    in_flight: &mut Vec<(usize, ToolFuture<'a>)>,
) -> (usize, Result<RichToolResult, ToolPortError>) {
    std::future::poll_fn(|context| {
        let mut index = 0;
        while index < in_flight.len() {
            match in_flight[index].1.as_mut().poll(context) {
                Poll::Ready(result) => {
                    let (call_index, _) = in_flight.swap_remove(index);
                    return Poll::Ready((call_index, result));
                }
                Poll::Pending => index += 1,
            }
        }
        Poll::Pending
    })
    .await
}
