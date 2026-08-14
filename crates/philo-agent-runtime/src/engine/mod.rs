//! The turn-driving engine: admission-to-settlement orchestration.
//!
//! `drive` owns the kernel conversation (BeginTurn, ModelCallCompleted,
//! ToolBatchCompleted) and the session barriers; the submodules own model
//! IO (`model_call`), tool IO (`tool_batch`), stale-turn sealing (`seal`),
//! and terminal settlement (`settlement`).

pub(crate) mod compaction;
mod model_call;
mod seal;
mod settlement;
mod stream;
mod tool_batch;

use crate::mapping::entries::{batch_entry, start_entries, success_entries};
use crate::mapping::failure::session_failure;
use crate::mapping::messages::context_messages;
use crate::mapping::parts::kernel_user_parts;
use crate::mapping::tool::kernel_result;
use crate::operation::{OperationPublisher, OperationShared, QueueClaim, Scheduler};
use crate::{
    AgentEvent, AgentFailure, AssistantMessage, ModelPort, OperationPhase, RunningToolBatchPhase,
    RuntimeConfig, SessionId, TurnSnapshot, UserMessage,
};
use philo_agent_kernel as kernel;
use philo_session as session;
use philo_tools::ToolPort;
use settlement::TurnCx;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

/// Owned dependencies threaded into an operation engine so queued operations
/// can be driven after `prompt()` has returned.
pub(crate) struct EngineContext {
    pub(crate) model: Arc<dyn ModelPort>,
    pub(crate) sessions: Arc<dyn session::SessionStore>,
    pub(crate) tools: Arc<dyn ToolPort>,
    pub(crate) config: RuntimeConfig,
    pub(crate) scheduler: Arc<Scheduler>,
    pub(crate) last_input_tokens: Arc<Mutex<HashMap<SessionId, u64>>>,
}

impl EngineContext {
    pub(crate) fn record_input_tokens(&self, session_id: &SessionId, input_tokens: u64) {
        self.last_input_tokens
            .lock()
            .expect("input usage mutex")
            .insert(session_id.clone(), input_tokens);
    }

    pub(crate) fn last_input_tokens(&self, session_id: &SessionId) -> Option<u64> {
        self.last_input_tokens
            .lock()
            .expect("input usage mutex")
            .get(session_id)
            .copied()
    }
}

/// Drives a claimed operation to settlement and releases the active slot.
pub(crate) async fn drive_claimed(
    ctx: EngineContext,
    shared: Arc<OperationShared>,
    session_id: SessionId,
    user_message: UserMessage,
) {
    let operation = OperationPublisher::begin(shared.clone(), ctx.config.operation_timeout);
    drive(&ctx, operation, session_id, user_message).await;
    ctx.scheduler.release(shared.operation_id());
}

/// Waits for the queue turn, then drives the operation. Completes without
/// driving when the operation was cancelled while still queued.
pub(crate) async fn run_queued(
    ctx: EngineContext,
    shared: Arc<OperationShared>,
    session_id: SessionId,
    user_message: UserMessage,
) {
    let claimed = std::future::poll_fn(|poll_context| {
        match ctx
            .scheduler
            .try_claim_queued(&shared, poll_context.waker())
        {
            QueueClaim::Claimed => std::task::Poll::Ready(true),
            QueueClaim::SettledInQueue => std::task::Poll::Ready(false),
            QueueClaim::NotYet => std::task::Poll::Pending,
        }
    })
    .await;
    if claimed {
        drive_claimed(ctx, shared, session_id, user_message).await;
    }
}

#[allow(clippy::too_many_lines)]
async fn drive(
    ctx: &EngineContext,
    operation: OperationPublisher,
    session_id: SessionId,
    user_message: UserMessage,
) {
    let stored_session_id = session::SessionId::new(session_id.as_str());
    let context = match ctx.sessions.context_view(&stored_session_id).await {
        Ok(context) => context,
        Err(error) => {
            operation.fail_unconfirmed(session_failure("reading session context", &error));
            return;
        }
    };
    // M11 seal step: every stale unfinished turn is sealed before this turn
    // starts; the returned view already contains the sealed facts.
    let (operation, context) =
        match seal::seal_stale_turns(ctx, operation, &stored_session_id, context).await {
            seal::SealOutcome::Sealed(operation, context) => (operation, context),
            seal::SealOutcome::Settled => return,
        };
    // M13 pre-turn maintenance: after all stale turns are sealed and before
    // TurnStarted / Barrier A, optionally replace an earlier settled prefix
    // with one durable summary.
    let (operation, context) = match compaction::maybe_auto_compact(
        ctx,
        operation,
        &session_id,
        &stored_session_id,
        context,
    )
    .await
    {
        compaction::AutoCompactionOutcome::Ready(operation, context) => (operation, context),
        compaction::AutoCompactionOutcome::Settled => return,
    };
    operation.turn_started();
    let turn = TurnSnapshot {
        session_id: session_id.clone(),
        session_revision: context.revision(),
        context_messages: context_messages(&context),
        system_prompt: ctx.config.system_prompt.clone(),
        model_target: ctx.config.model_target.clone(),
        generation: ctx.config.generation.clone(),
        tools: ctx.tools.definitions(),
        max_tool_rounds: ctx.config.max_tool_rounds,
    };
    // Explicit runtime -> kernel mapping; both layers enforce the same
    // structural rules, so a constructed UserMessage always converts.
    let kernel_user = match kernel::UserMessage::from_parts(kernel_user_parts(user_message.parts()))
    {
        Ok(user) => user,
        Err(_) => {
            operation.fail_unconfirmed(AgentFailure::runtime_driver(
                "kernel rejected user message parts",
            ));
            return;
        }
    };
    let initial = kernel::initial_state();
    let started = match kernel::transition(
        &initial,
        kernel::KernelInput::BeginTurn {
            turn_id: kernel::TurnId::new(operation.turn_id().as_str()),
            user_message: kernel_user,
            max_tool_rounds: turn.max_tool_rounds,
        },
    ) {
        Ok(value) => value,
        Err(_) => {
            operation.fail_unconfirmed(AgentFailure::runtime_driver("kernel rejected BeginTurn"));
            return;
        }
    };
    let entries = match start_entries(
        &started.observations,
        operation.operation_id(),
        operation.turn_id(),
    ) {
        Ok(entries) => entries,
        Err(failure) => {
            operation.fail_unconfirmed(failure);
            return;
        }
    };
    // Injection point: cancel before Barrier A persists ends with zero
    // persistent trace; no turn exists durably.
    if operation.is_cancel_requested() {
        operation.cancellation_observed();
        operation.cancel_zero_trace();
        return;
    }
    let start_commit = match ctx
        .sessions
        .commit(session::SessionTransaction::linear(
            stored_session_id.clone(),
            context.revision(),
            entries,
        ))
        .await
    {
        Ok(commit) => commit,
        Err(error) => {
            operation.fail_unconfirmed(session_failure("committing turn start", &error));
            return;
        }
    };
    let mut cx = TurnCx {
        ctx,
        operation,
        session_id: stored_session_id,
        revision: start_commit.revision(),
        state: started.next_state,
    };
    let mut effect = started.effect.expect("BeginTurn always requests model");
    let mut model_call_index: u32 = 0;
    loop {
        match effect {
            kernel::KernelEffect::RequestModel {
                effect_id,
                model_call_id,
                turn_messages: messages,
                tools_allowed,
            } => {
                model_call_index += 1;
                let step = model_call::run(
                    cx,
                    &turn,
                    &effect_id,
                    &model_call_id,
                    model_call_index,
                    messages,
                    tools_allowed,
                )
                .await;
                let Some((next_cx, text, calls)) = step else {
                    return;
                };
                cx = next_cx;
                let output = if calls.is_empty() {
                    kernel::AssistantOutput::final_text(&text)
                } else {
                    kernel::AssistantOutput::tool_calls(calls)
                };
                let completed = match kernel::transition(
                    &cx.state,
                    kernel::KernelInput::ModelCallCompleted {
                        effect_id: effect_id.clone(),
                        output,
                    },
                ) {
                    Ok(value) => value,
                    Err(_) => {
                        cx.fail(
                            effect_id,
                            AgentFailure::invalid_model_output("kernel rejected model output"),
                        )
                        .await;
                        return;
                    }
                };
                if !completed.observations.iter().any(|observation| {
                    matches!(
                        observation,
                        kernel::KernelObservation::AssistantToolCallsAccepted { .. }
                    )
                }) {
                    // The kernel made its final decision: cancellation can
                    // no longer take effect on this operation.
                    settle_success(cx, effect_id, &completed.observations, text).await;
                    return;
                }
                let (tool_effect_id, batch_id, calls) = match completed.effect.as_ref() {
                    Some(kernel::KernelEffect::ExecuteToolBatch {
                        effect_id,
                        tool_batch_id,
                        calls,
                    }) => (effect_id.clone(), tool_batch_id.clone(), calls.clone()),
                    _ => {
                        cx.fail(
                            effect_id,
                            AgentFailure::runtime_driver(
                                "tool-call transition omitted tool effect",
                            ),
                        )
                        .await;
                        return;
                    }
                };
                cx.operation.set_phase(OperationPhase::RunningToolBatch(
                    RunningToolBatchPhase::Preparing,
                ));
                let entry = batch_entry(cx.operation.turn_id(), &model_call_id, &batch_id, &calls);
                let committed = cx
                    .ctx
                    .sessions
                    .commit(session::SessionTransaction::linear(
                        cx.session_id.clone(),
                        cx.revision,
                        vec![entry],
                    ))
                    .await;
                let batch_commit = match committed {
                    Ok(commit) => commit,
                    Err(error) => {
                        cx.fail(effect_id, session_failure("committing tool calls", &error))
                            .await;
                        return;
                    }
                };
                cx.revision = batch_commit.revision();
                cx.state = completed.next_state;
                cx.operation.push(AgentEvent::ToolBatchRequested {
                    tool_batch_id: crate::ToolBatchId::new(batch_id.as_str()),
                    call_count: calls.len(),
                });
                let step = tool_batch::run(cx, &tool_effect_id, &batch_id, &calls).await;
                let Some((next_cx, results)) = step else {
                    return;
                };
                cx = next_cx;
                let kernel_results = results
                    .iter()
                    .map(|(call, rich)| kernel_result(call, rich.result()))
                    .collect();
                let next = match kernel::transition(
                    &cx.state,
                    kernel::KernelInput::ToolBatchCompleted {
                        effect_id: tool_effect_id,
                        results: kernel_results,
                    },
                ) {
                    Ok(value) => value,
                    Err(_) => {
                        cx.operation
                            .fail_unconfirmed(AgentFailure::invalid_model_output(
                                "kernel rejected tool results",
                            ));
                        return;
                    }
                };
                cx.state = next.next_state;
                effect = next.effect.expect("tool completion requests model");
            }
            kernel::KernelEffect::ExecuteToolBatch { .. } => {
                cx.operation.fail_unconfirmed(AgentFailure::runtime_driver(
                    "runtime cannot execute uncommitted tool effect",
                ));
                return;
            }
        }
    }
}

/// Commits the successful settlement of a final text answer and resolves
/// the operation; commit failure takes the established failure path over
/// the old outstanding effect.
async fn settle_success(
    cx: TurnCx<'_>,
    effect_id: kernel::EffectId,
    observations: &[kernel::KernelObservation],
    text: String,
) {
    let final_entries = match success_entries(
        observations,
        cx.operation.operation_id(),
        cx.operation.turn_id(),
    ) {
        Ok(entries) => entries,
        Err(failure) => {
            cx.fail(effect_id, failure).await;
            return;
        }
    };
    cx.operation.set_phase(OperationPhase::Finalizing);
    let committed = cx
        .ctx
        .sessions
        .commit(session::SessionTransaction::linear(
            cx.session_id.clone(),
            cx.revision,
            final_entries,
        ))
        .await;
    if let Err(error) = committed {
        cx.fail(
            effect_id,
            session_failure("committing successful settlement", &error),
        )
        .await;
        return;
    }
    cx.operation.succeed(AssistantMessage { content: text });
}
