//! One logical model call: request build, cancellable stream consumption,
//! and output assembly.
//!
//! A failed attempt is returned to the caller instead of settling here so
//! the turn engine can apply its bounded recovery policy; only cancellation
//! settles inside this module.

use super::settlement::TurnCx;
use super::stream::{OutputAssembler, StreamStep, next_or_cancel};
use crate::mapping::messages::build_messages;
use crate::{
    AgentEvent, AgentFailure, AgentFailureKind, ModelAssistantBlock, ModelCallPhase,
    ModelCallSnapshot, ModelError, ModelEvent, OperationPhase, ToolCallDelta, TurnSnapshot,
};
use philo_agent_kernel as kernel;

/// Why one model-call attempt failed and whether an identical re-attempt
/// can succeed. `recoverable` mirrors [`ModelError`]'s delivery-fault
/// classification; every structural failure is permanent by construction.
pub(super) struct AttemptFailure {
    pub(super) failure: AgentFailure,
    pub(super) recoverable: bool,
}

impl AttemptFailure {
    fn classify(error: &ModelError) -> Self {
        Self {
            failure: AgentFailure::new(AgentFailureKind::ModelCall, error.message()),
            recoverable: error.class() == crate::ModelFailureClass::Recoverable,
        }
    }

    fn permanent(failure: AgentFailure) -> Self {
        Self {
            failure,
            recoverable: false,
        }
    }
}

/// Outcome of one logical model call.
pub(super) enum ModelCallOutcome<'a> {
    /// Authoritative assembled output; the caller owns the advanced context.
    Completed(TurnCx<'a>, Vec<ModelAssistantBlock>),
    /// The attempt failed; nothing was settled and nothing durable was
    /// committed for this call. The caller retries or fails the operation.
    Failed(TurnCx<'a>, AttemptFailure),
    /// Cancellation was observed and settled inside; stop driving.
    Settled,
}

/// Runs one logical model call. On success returns the context with the
/// authoritative `Completed.blocks`.
pub(super) async fn run<'a>(
    cx: TurnCx<'a>,
    turn: &TurnSnapshot,
    effect_id: &kernel::EffectId,
    model_call_id: &kernel::ModelCallId,
    model_call_index: u32,
    messages: Vec<kernel::TurnMessage>,
    tools_allowed: bool,
) -> ModelCallOutcome<'a> {
    // Injection point: between barriers, before the next model call starts.
    // The latest batch is already fully committed, so the cancellation
    // transaction is terminal-only.
    if cx.operation.is_cancel_requested() {
        cx.operation.cancellation_observed().await;
        cx.cancel(effect_id.clone(), Vec::new(), Vec::new()).await;
        return ModelCallOutcome::Settled;
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
            return ModelCallOutcome::Failed(cx, AttemptFailure::classify(&error));
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
                return ModelCallOutcome::Settled;
            }
            StreamStep::Event(Some(Ok(event))) => match (completed_seen, event) {
                (true, ModelEvent::Completed { .. }) => {
                    return ModelCallOutcome::Failed(
                        cx,
                        AttemptFailure::permanent(AgentFailure::invalid_model_output(
                            "model stream emitted Completed more than once",
                        )),
                    );
                }
                (true, _) => {
                    return ModelCallOutcome::Failed(
                        cx,
                        AttemptFailure::permanent(AgentFailure::invalid_model_output(
                            "model stream emitted output after Completed",
                        )),
                    );
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
                        return ModelCallOutcome::Failed(
                            cx,
                            AttemptFailure::permanent(AgentFailure::invalid_model_output(
                                "model stream emitted ResponseStarted more than once",
                            )),
                        );
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
                return ModelCallOutcome::Failed(cx, AttemptFailure::classify(&error));
            }
            StreamStep::Event(None) if completed_seen => break,
            StreamStep::Event(None) => {
                return ModelCallOutcome::Failed(
                    cx,
                    AttemptFailure::permanent(AgentFailure::runtime_driver(
                        "model stream ended before Completed",
                    )),
                );
            }
        }
    }
    ModelCallOutcome::Completed(
        cx,
        completed_blocks.expect("Completed was observed before the stream ended"),
    )
}
