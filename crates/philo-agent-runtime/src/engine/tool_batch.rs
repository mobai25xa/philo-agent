//! One committed tool batch: serial or bounded-parallel execution, then the
//! results barrier.

use super::settlement::{BatchSlot, TurnCx, cancellation_batch};
use super::tool_progress::ToolProgressBridge;
use crate::mapping::failure::commit_failure;
use crate::mapping::tool::session_result;
use crate::{
    AgentEvent, AgentFailure, FailureDomain, FailureStage, OperationPhase, RetryDisposition,
    RunningToolBatchPhase,
};
use philo_agent_kernel as kernel;
use philo_session as session;
use philo_tools::{
    RichToolResult, ToolFuture, ToolInvocation, ToolInvokeCx, ToolInvokeEnd, ToolPortError,
};
use std::task::Poll;
use std::time::{Duration, Instant};

/// ToolPort infrastructure failure (`tool.port_failed`): side effects are
/// not provably absent, so the recorded advice is MayDuplicate.
fn port_failure(error: &ToolPortError) -> AgentFailure {
    AgentFailure::new(
        "tool.port_failed",
        FailureDomain::Internal,
        FailureStage::ToolPort,
        RetryDisposition::MayDuplicate { retry_after_ms: None },
        "a tool execution infrastructure fault occurred",
        error.message(),
    )
}

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

async fn run_serial<'a>(
    cx: TurnCx<'a>,
    effect_id: &kernel::EffectId,
    batch_id: &kernel::ToolBatchId,
    calls: &[kernel::KernelToolCall],
) -> Option<(TurnCx<'a>, Vec<(kernel::KernelToolCall, RichToolResult)>)> {
    let grace = cx.ctx.config().tool_cancel_grace;
    let mut slots: Vec<BatchSlot> = calls.iter().map(|_| BatchSlot::Unstarted).collect();
    for (index, call) in calls.iter().enumerate() {
        if cx.operation.is_cancel_requested() {
            cx.operation.cancellation_observed().await;
            let (marks, executed_events) =
                cancellation_batch(cx.operation.turn_id(), batch_id, calls, &slots);
            cx.cancel(effect_id.clone(), marks, executed_events).await;
            return None;
        }
        cx.operation.set_phase(OperationPhase::RunningToolBatch(
            RunningToolBatchPhase::Executing {
                in_flight: 1,
                completed: index,
            },
        ));
        publish_started(&cx, batch_id, call, index).await;
        match await_invoke(&cx, batch_id, call, index, grace).await {
            InvokeWait::Finished(ToolInvokeEnd::Done(result)) => {
                slots[index] = BatchSlot::Done(result);
            }
            InvokeWait::Finished(ToolInvokeEnd::Stopped) | InvokeWait::Dropped => {
                if !cx.operation.is_cancel_requested() {
                    use FailureDomain as D;
                    use FailureStage as S;
                    use RetryDisposition as R;
                    cx.fail(
                        effect_id.clone(),
                        AgentFailure::new(
                            "tool.stopped_without_cancel",
                            D::Internal,
                            S::ToolPort,
                            R::Never,
                            "a tool stopped on its own without a cancel request",
                            "tool stopped without a cancel request",
                        ),
                    )
                    .await;
                    return None;
                }
                slots[index] = BatchSlot::Stopped;
                cx.operation.cancellation_observed().await;
                let (marks, executed_events) =
                    cancellation_batch(cx.operation.turn_id(), batch_id, calls, &slots);
                cx.cancel(effect_id.clone(), marks, executed_events).await;
                return None;
            }
            InvokeWait::PortError(error) => {
                if cx.operation.is_cancel_requested() {
                    slots[index] = BatchSlot::Stopped;
                    cx.operation.cancellation_observed().await;
                    let (marks, executed_events) =
                        cancellation_batch(cx.operation.turn_id(), batch_id, calls, &slots);
                    cx.cancel(effect_id.clone(), marks, executed_events).await;
                    return None;
                }
                cx.fail(effect_id.clone(), port_failure(&error)).await;
                return None;
            }
        }
    }
    let results = done_results(calls, &slots);
    commit_results(cx, effect_id, batch_id, results).await
}

async fn run_parallel<'a>(
    cx: TurnCx<'a>,
    effect_id: &kernel::EffectId,
    batch_id: &kernel::ToolBatchId,
    calls: &[kernel::KernelToolCall],
    limit: usize,
) -> Option<(TurnCx<'a>, Vec<(kernel::KernelToolCall, RichToolResult)>)> {
    let tools = cx.ctx.tools().clone();
    let grace = cx.ctx.config().tool_cancel_grace;
    let mut slots: Vec<BatchSlot> = calls.iter().map(|_| BatchSlot::Unstarted).collect();
    let mut in_flight: Vec<(usize, ToolFuture<'static>)> = Vec::new();
    let mut next = 0;
    let mut stop_starting = false;
    let mut port_error: Option<ToolPortError> = None;
    let mut grace_deadline: Option<Instant> = None;

    loop {
        if cx.operation.is_cancel_requested() {
            stop_starting = true;
            if grace_deadline.is_none() {
                grace_deadline = Some(Instant::now() + grace);
            }
        }
        while !stop_starting && in_flight.len() < limit && next < calls.len() {
            if cx.operation.is_cancel_requested() {
                stop_starting = true;
                break;
            }
            let index = next;
            let call = &calls[index];
            next += 1;
            set_executing(&cx, in_flight.len() + 1, completed_count(&slots));
            publish_started(&cx, batch_id, call, index).await;
            in_flight.push((
                index,
                invoke_call_future(
                    tools.clone(),
                    cx.operation.shared_arc(),
                    batch_id,
                    call,
                    index,
                ),
            ));
        }
        if in_flight.is_empty() {
            break;
        }
        match next_finished(&mut in_flight, grace_deadline).await {
            Some((index, Ok(ToolInvokeEnd::Done(result)))) => {
                slots[index] = BatchSlot::Done(result);
            }
            Some((index, Ok(ToolInvokeEnd::Stopped))) => {
                if cx.operation.is_cancel_requested() {
                    slots[index] = BatchSlot::Stopped;
                    stop_starting = true;
                } else {
                    port_error = Some(ToolPortError::new("tool stopped without a cancel request"));
                    stop_starting = true;
                }
            }
            Some((index, Err(error))) => {
                if cx.operation.is_cancel_requested() {
                    slots[index] = BatchSlot::Stopped;
                    stop_starting = true;
                } else {
                    port_error = Some(error);
                    stop_starting = true;
                }
            }
            None => {
                for (index, _future) in in_flight.drain(..) {
                    slots[index] = BatchSlot::Stopped;
                }
                break;
            }
        }
        set_executing(&cx, in_flight.len(), completed_count(&slots));
    }

    if let Some(error) = port_error {
        cx.fail(effect_id.clone(), port_failure(&error)).await;
        return None;
    }

    if slots.iter().any(|slot| !matches!(slot, BatchSlot::Done(_)))
        || cx.operation.is_cancel_requested()
    {
        cx.operation.cancellation_observed().await;
        let (marks, executed_events) =
            cancellation_batch(cx.operation.turn_id(), batch_id, calls, &slots);
        cx.cancel(effect_id.clone(), marks, executed_events).await;
        return None;
    }
    let results = done_results(calls, &slots);
    commit_results(cx, effect_id, batch_id, results).await
}

async fn commit_results<'a>(
    mut cx: TurnCx<'a>,
    effect_id: &kernel::EffectId,
    batch_id: &kernel::ToolBatchId,
    results: Vec<(kernel::KernelToolCall, RichToolResult)>,
) -> Option<(TurnCx<'a>, Vec<(kernel::KernelToolCall, RichToolResult)>)> {
    if cx.operation.is_cancel_requested() {
        cx.operation.cancellation_observed().await;
        let slots: Vec<BatchSlot> = results
            .iter()
            .map(|(_, rich)| BatchSlot::Done(rich.clone()))
            .collect();
        let calls: Vec<_> = results.iter().map(|(call, _)| call.clone()).collect();
        let (marks, executed_events) =
            cancellation_batch(cx.operation.turn_id(), batch_id, &calls, &slots);
        cx.cancel(effect_id.clone(), marks, executed_events).await;
        return None;
    }
    cx.operation.set_phase(OperationPhase::RunningToolBatch(
        RunningToolBatchPhase::CommittingResults,
    ));
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
                commit_failure(
                    "engine.barrier_c_commit_failed",
                    RetryDisposition::MayDuplicate { retry_after_ms: None },
                    "committing tool results",
                    &error,
                ),
            )
            .await;
            return None;
        }
    };
    for (index, (call, rich)) in results.iter().enumerate() {
        cx.operation
            .push(AgentEvent::ToolExecutionCompleted {
                tool_batch_id: crate::ToolBatchId::new(batch_id.as_str()),
                tool_call_id: crate::ToolCallId::new(call.id().as_str()),
                index,
                tool_name: call.name().to_owned(),
                result: rich.result().clone(),
                display: rich.display().cloned(),
            })
            .await;
    }
    cx.revision = results_commit.revision();
    cx.current_leaf = results_commit.current_leaf().clone();
    Some((cx, results))
}

enum InvokeWait {
    Finished(ToolInvokeEnd),
    Dropped,
    PortError(ToolPortError),
}

async fn await_invoke(
    cx: &TurnCx<'_>,
    batch_id: &kernel::ToolBatchId,
    call: &kernel::KernelToolCall,
    index: usize,
    grace: Duration,
) -> InvokeWait {
    let mut future = invoke_call_future(
        cx.ctx.tools().clone(),
        cx.operation.shared_arc(),
        batch_id,
        call,
        index,
    );
    let shared = cx.operation.shared_arc();
    tokio::select! {
        biased;
        result = &mut future => match result {
            Ok(end) => InvokeWait::Finished(end),
            Err(error) => InvokeWait::PortError(error),
        },
        _ = wait_cancel_grace(&shared, grace) => InvokeWait::Dropped,
    }
}

async fn wait_cancel_grace(shared: &crate::operation::OperationShared, grace: Duration) {
    shared.wait_until_cancelled().await;
    if !grace.is_zero() {
        tokio::time::sleep(grace).await;
    }
}

fn invoke_call_future(
    tools: std::sync::Arc<dyn philo_tools::ToolPort>,
    shared: std::sync::Arc<crate::operation::OperationShared>,
    batch_id: &kernel::ToolBatchId,
    call: &kernel::KernelToolCall,
    index: usize,
) -> ToolFuture<'static> {
    let invocation = ToolInvocation::new(call.id().as_str(), call.name(), call.arguments());
    let cancel = shared.tool_cancel();
    let (bridge, sink) = ToolProgressBridge::new(
        shared,
        crate::ToolBatchId::new(batch_id.as_str()),
        crate::ToolCallId::new(call.id().as_str()),
        index,
    );
    Box::pin(async move {
        let invoked = tools
            .invoke(invocation, ToolInvokeCx::new(sink, cancel))
            .await;
        bridge.finish();
        invoked
    })
}

async fn publish_started(
    cx: &TurnCx<'_>,
    batch_id: &kernel::ToolBatchId,
    call: &kernel::KernelToolCall,
    index: usize,
) {
    cx.operation
        .push(AgentEvent::ToolExecutionStarted {
            tool_batch_id: crate::ToolBatchId::new(batch_id.as_str()),
            tool_call_id: crate::ToolCallId::new(call.id().as_str()),
            index,
            tool_name: call.name().to_owned(),
            arguments: call.arguments().to_owned(),
        })
        .await;
}

fn set_executing(cx: &TurnCx<'_>, in_flight: usize, completed: usize) {
    cx.operation.set_phase(OperationPhase::RunningToolBatch(
        RunningToolBatchPhase::Executing {
            in_flight,
            completed,
        },
    ));
}

fn completed_count(slots: &[BatchSlot]) -> usize {
    slots
        .iter()
        .filter(|slot| !matches!(slot, BatchSlot::Unstarted))
        .count()
}

fn done_results(
    calls: &[kernel::KernelToolCall],
    slots: &[BatchSlot],
) -> Vec<(kernel::KernelToolCall, RichToolResult)> {
    calls
        .iter()
        .zip(slots.iter())
        .filter_map(|(call, slot)| match slot {
            BatchSlot::Done(result) => Some((call.clone(), result.clone())),
            _ => None,
        })
        .collect()
}

async fn next_finished(
    in_flight: &mut Vec<(usize, ToolFuture<'static>)>,
    deadline: Option<Instant>,
) -> Option<(usize, Result<ToolInvokeEnd, ToolPortError>)> {
    tokio::select! {
        biased;
        result = poll_next_ready(in_flight) => result,
        _ = async {
            match deadline {
                Some(at) => tokio::time::sleep(at.saturating_duration_since(Instant::now())).await,
                None => std::future::pending().await,
            }
        } => None,
    }
}

async fn poll_next_ready(
    in_flight: &mut Vec<(usize, ToolFuture<'static>)>,
) -> Option<(usize, Result<ToolInvokeEnd, ToolPortError>)> {
    std::future::poll_fn(|context| {
        let mut index = 0;
        while index < in_flight.len() {
            match in_flight[index].1.as_mut().poll(context) {
                Poll::Ready(result) => {
                    let (call_index, _) = in_flight.swap_remove(index);
                    return Poll::Ready(Some((call_index, result)));
                }
                Poll::Pending => index += 1,
            }
        }
        Poll::Pending
    })
    .await
}
