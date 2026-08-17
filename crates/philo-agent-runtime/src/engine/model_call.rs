//! One logical model call: request build, cancellable stream consumption,
//! and output assembly.

use super::settlement::TurnCx;
use super::stream::{OutputAssembler, StreamStep, next_or_cancel};
use crate::mapping::messages::build_messages;
use crate::{
    AgentEvent, AgentFailure, AgentFailureKind, ModelAssistantBlock, ModelCallPhase,
    ModelCallSnapshot, ModelEvent, OperationPhase, ToolCallDelta, TurnSnapshot,
};
use philo_agent_kernel as kernel;

/// Runs one logical model call and returns the context with the
/// authoritative `Completed.blocks`; `None` when a failure or cancellation
/// already settled the operation.
pub(super) async fn run<'a>(
    cx: TurnCx<'a>,
    turn: &TurnSnapshot,
    effect_id: &kernel::EffectId,
    model_call_id: &kernel::ModelCallId,
    model_call_index: u32,
    messages: Vec<kernel::TurnMessage>,
    tools_allowed: bool,
) -> Option<(TurnCx<'a>, Vec<ModelAssistantBlock>)> {
    // Injection point: between barriers, before the next model call starts.
    // The latest batch is already fully committed, so the cancellation
    // transaction is terminal-only.
    if cx.operation.is_cancel_requested() {
        cx.operation.cancellation_observed().await;
        cx.cancel(effect_id.clone(), Vec::new(), Vec::new()).await;
        return None;
    }
    cx.operation
        .set_phase(OperationPhase::RunningModelCall(ModelCallPhase::Starting));
    cx.operation
        .push(AgentEvent::ModelCallStarted {
            model_call_id: crate::ModelCallId::new(model_call_id.as_str()),
        })
        .await;
    let request = ModelCallSnapshot {
        session_id: turn.session_id.clone(),
        context_fingerprint: cx.current_leaf.as_str().to_owned(),
        persist_replay: true,
        operation_id: cx.operation.operation_id().clone(),
        turn_id: cx.operation.turn_id().clone(),
        model_call_id: crate::ModelCallId::new(model_call_id.as_str()),
        model_call_index,
        session_revision: cx.revision,
        messages: build_messages(turn, messages),
        tools: if tools_allowed {
            turn.tools.clone()
        } else {
            Vec::new()
        },
        model_target: turn.model_target.clone(),
        generation: turn.generation.clone(),
        max_parallel_tool_calls: turn.max_parallel_tool_calls,
    };
    let started = cx.ctx.model().start(request).await;
    let mut stream = match started {
        Ok(stream) => stream,
        Err(error) => {
            cx.fail(
                effect_id.clone(),
                AgentFailure::new(AgentFailureKind::ModelCall, error.message()),
            )
            .await;
            return None;
        }
    };
    cx.operation.set_phase(OperationPhase::RunningModelCall(
        ModelCallPhase::WaitingForFirstOutput,
    ));
    let mut assembler = OutputAssembler::default();
    let mut completed_seen = false;
    let mut completed_blocks = None;
    let mut response_started_seen = false;
    loop {
        let step = next_or_cancel(stream.as_mut(), cx.operation.shared()).await;
        match step {
            // Injection point: cancel during the model stream. Dropping the
            // stream is the ModelPort cancellation signal; published text
            // deltas stay transient facts.
            StreamStep::CancelObserved => {
                drop(stream);
                cx.operation.cancellation_observed().await;
                cx.cancel(effect_id.clone(), Vec::new(), Vec::new()).await;
                return None;
            }
            StreamStep::Event(Some(Ok(event))) => match (completed_seen, event) {
                (true, ModelEvent::Completed { .. }) => {
                    cx.fail(
                        effect_id.clone(),
                        AgentFailure::invalid_model_output(
                            "model stream emitted Completed more than once",
                        ),
                    )
                    .await;
                    return None;
                }
                (true, _) => {
                    cx.fail(
                        effect_id.clone(),
                        AgentFailure::invalid_model_output(
                            "model stream emitted output after Completed",
                        ),
                    )
                    .await;
                    return None;
                }
                (false, ModelEvent::Completed { blocks }) => {
                    completed_seen = true;
                    completed_blocks = Some(blocks);
                }
                (
                    false,
                    ModelEvent::ResponseStarted {
                        response_model,
                        response_id,
                    },
                ) => {
                    if response_started_seen {
                        cx.fail(
                            effect_id.clone(),
                            AgentFailure::invalid_model_output(
                                "model stream emitted ResponseStarted more than once",
                            ),
                        )
                        .await;
                        return None;
                    }
                    response_started_seen = true;
                    cx.operation
                        .push(AgentEvent::ModelResponseStarted {
                            model_call_id: crate::ModelCallId::new(model_call_id.as_str()),
                            response_model,
                            response_id,
                        })
                        .await;
                }
                (false, ModelEvent::TextDelta(delta)) => {
                    cx.operation
                        .set_phase(OperationPhase::RunningModelCall(ModelCallPhase::Streaming));
                    assembler.text(&delta);
                    cx.operation.push(AgentEvent::TextDelta { delta }).await;
                }
                // Transient observations: forwarded as-is, never part of
                // the assembled output, never written to the Session.
                (false, ModelEvent::ReasoningDelta { text }) => {
                    cx.operation
                        .push(AgentEvent::ReasoningDelta {
                            model_call_id: crate::ModelCallId::new(model_call_id.as_str()),
                            text,
                        })
                        .await;
                }
                (false, ModelEvent::UsageUpdated { usage }) => {
                    if let Some(input_tokens) = usage.input_tokens {
                        cx.ctx.record_input_tokens(&turn.session_id, input_tokens);
                    }
                    cx.operation
                        .push(AgentEvent::ModelUsageUpdated {
                            model_call_id: crate::ModelCallId::new(model_call_id.as_str()),
                            usage,
                        })
                        .await;
                }
                (
                    false,
                    ModelEvent::ToolCallDelta {
                        index,
                        id,
                        name,
                        arguments,
                    },
                ) => {
                    assembler.tool(ToolCallDelta {
                        index,
                        id,
                        name,
                        arguments,
                    });
                }
            },
            StreamStep::Event(Some(Err(error))) => {
                cx.fail(
                    effect_id.clone(),
                    AgentFailure::new(AgentFailureKind::ModelCall, error.message()),
                )
                .await;
                return None;
            }
            StreamStep::Event(None) if completed_seen => break,
            StreamStep::Event(None) => {
                cx.fail(
                    effect_id.clone(),
                    AgentFailure::runtime_driver("model stream ended before Completed"),
                )
                .await;
                return None;
            }
        }
    }
    Some((
        cx,
        completed_blocks.expect("Completed was observed before the stream ended"),
    ))
}
