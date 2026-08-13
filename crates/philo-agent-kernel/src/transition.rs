use crate::phase;
use crate::protocol::*;
use crate::state::{KernelState, State};
use crate::tool_loop::{matching_results, valid_calls};

pub fn transition(
    state: &KernelState,
    input: KernelInput,
) -> Result<KernelTransition, KernelInputRejection> {
    match (&state.inner, input) {
        (
            State::ExpectingTurnStart,
            KernelInput::BeginTurn {
                turn_id,
                user_message,
                max_tool_rounds,
            },
        ) => Ok(begin(turn_id, user_message, max_tool_rounds)),
        (
            State::ExpectingModelCompletion {
                effect_id,
                model_call_id,
                used_rounds,
                max_tool_rounds,
                turn_id,
                transcript,
            },
            KernelInput::ModelCallCompleted {
                effect_id: received,
                output,
            },
        ) => complete_model(
            effect_id,
            model_call_id,
            *used_rounds,
            *max_tool_rounds,
            turn_id,
            transcript,
            received,
            output,
        ),
        (
            State::ExpectingToolBatchCompletion {
                effect_id,
                tool_batch_id,
                round,
                max_tool_rounds,
                turn_id,
                transcript,
                calls,
            },
            KernelInput::ToolBatchCompleted {
                effect_id: received,
                results,
            },
        ) => complete_tools(
            effect_id,
            tool_batch_id,
            *round,
            *max_tool_rounds,
            turn_id,
            transcript,
            calls,
            received,
            results,
        ),
        (
            State::ExpectingModelCompletion { effect_id, .. }
            | State::ExpectingToolBatchCompletion { effect_id, .. },
            KernelInput::TerminationRequested {
                effect_id: received,
                failure,
            },
        ) => terminate(state, effect_id, received, failure),
        (
            State::ExpectingModelCompletion { effect_id, .. }
            | State::ExpectingToolBatchCompletion { effect_id, .. },
            KernelInput::CancelRequested {
                effect_id: received,
            },
        ) => cancel(state, effect_id, received),
        (
            State::Terminated {
                completed_effect_id,
                ..
            },
            KernelInput::ModelCallCompleted { effect_id, .. }
            | KernelInput::ToolBatchCompleted { effect_id, .. },
        ) if effect_id == *completed_effect_id => reject(
            state,
            KernelInputRejectionReason::EffectAlreadyCompleted { effect_id },
        ),
        (State::Terminated { .. }, _) => {
            reject(state, KernelInputRejectionReason::KernelTerminated)
        }
        _ => reject(state, KernelInputRejectionReason::InputNotAccepted),
    }
}

/// Deterministic identifiers: `RequestModel` #k uses effect sequence `2k-1`,
/// `ExecuteToolBatch` #k uses effect sequence `2k`.
fn model_call_ids(turn_id: &TurnId, call_number: u32) -> (ModelCallId, EffectId) {
    (
        ModelCallId::new(format!("{turn_id}:model-call:{call_number}")),
        EffectId::new(format!("{}:effect:{}", turn_id, 2 * call_number - 1)),
    )
}

fn tool_batch_ids(turn_id: &TurnId, round: u32) -> (ToolBatchId, EffectId) {
    (
        ToolBatchId::new(format!("{turn_id}:tool-batch:{round}")),
        EffectId::new(format!("{}:effect:{}", turn_id, 2 * round)),
    )
}

fn begin(turn_id: TurnId, user: UserMessage, max_tool_rounds: u32) -> KernelTransition {
    let (model_id, effect_id) = model_call_ids(&turn_id, 1);
    let transcript = vec![TurnMessage::User(user.clone())];
    let next_state = KernelState {
        inner: State::ExpectingModelCompletion {
            effect_id: effect_id.clone(),
            model_call_id: model_id.clone(),
            used_rounds: 0,
            max_tool_rounds,
            turn_id: turn_id.clone(),
            transcript: transcript.clone(),
        },
    };
    KernelTransition {
        phase: phase(&next_state),
        next_state,
        observations: vec![
            KernelObservation::TurnBegan {
                turn_id,
                user_message: user,
            },
            KernelObservation::ModelCallRequested {
                model_call_id: model_id.clone(),
                effect_id: effect_id.clone(),
            },
        ],
        durability: DurabilityRequirement::BeforeNextEffect,
        effect: Some(KernelEffect::RequestModel {
            effect_id,
            model_call_id: model_id,
            turn_messages: transcript,
            tools_allowed: 0 < max_tool_rounds,
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn complete_model(
    expected: &EffectId,
    model_id: &ModelCallId,
    used_rounds: u32,
    max_tool_rounds: u32,
    turn_id: &TurnId,
    transcript: &[TurnMessage],
    received: EffectId,
    output: AssistantOutput,
) -> Result<KernelTransition, KernelInputRejection> {
    let current = KernelPhaseView::ExpectingModelCompletion {
        effect_id: expected.clone(),
        model_call_id: model_id.clone(),
    };
    if received != *expected {
        return mismatch(current, expected, received);
    }
    if output.is_unsupported() {
        return Err(KernelInputRejection {
            phase: current,
            reason: KernelInputRejectionReason::UnsupportedAssistantOutput,
        });
    }
    if let Some(calls) = output.tool_call_batch() {
        let tools_allowed = used_rounds < max_tool_rounds;
        if !tools_allowed {
            return Err(KernelInputRejection {
                phase: current,
                reason: KernelInputRejectionReason::UnsupportedAssistantOutput,
            });
        }
        if !output.text().is_empty() || !valid_calls(calls) {
            return Err(KernelInputRejection {
                phase: current,
                reason: KernelInputRejectionReason::InvalidToolCalls,
            });
        }
        let round = used_rounds + 1;
        let (batch_id, tool_effect) = tool_batch_ids(turn_id, round);
        let calls = calls.to_vec();
        let mut next_transcript = transcript.to_vec();
        next_transcript.push(TurnMessage::AssistantToolCalls {
            tool_batch_id: batch_id.clone(),
            calls: calls.clone(),
        });
        let next_state = KernelState {
            inner: State::ExpectingToolBatchCompletion {
                effect_id: tool_effect.clone(),
                tool_batch_id: batch_id.clone(),
                round,
                max_tool_rounds,
                turn_id: turn_id.clone(),
                transcript: next_transcript,
                calls: calls.clone(),
            },
        };
        return Ok(KernelTransition {
            phase: phase(&next_state),
            next_state,
            observations: vec![
                KernelObservation::AssistantToolCallsAccepted {
                    model_call_id: model_id.clone(),
                    tool_batch_id: batch_id.clone(),
                    calls: calls.clone(),
                },
                KernelObservation::ToolBatchRequested {
                    tool_batch_id: batch_id.clone(),
                    effect_id: tool_effect.clone(),
                },
            ],
            durability: DurabilityRequirement::BeforeNextEffect,
            effect: Some(KernelEffect::ExecuteToolBatch {
                effect_id: tool_effect,
                tool_batch_id: batch_id,
                calls,
            }),
        });
    }
    let next_state = KernelState {
        inner: State::Terminated {
            outcome: TurnOutcome::Succeeded,
            completed_effect_id: expected.clone(),
        },
    };
    Ok(KernelTransition {
        phase: phase(&next_state),
        next_state,
        observations: vec![
            KernelObservation::AssistantOutputAccepted {
                model_call_id: model_id.clone(),
                output,
            },
            KernelObservation::TurnTerminated {
                outcome: TurnOutcome::Succeeded,
            },
        ],
        durability: DurabilityRequirement::BeforeSettlement,
        effect: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn complete_tools(
    expected: &EffectId,
    batch_id: &ToolBatchId,
    round: u32,
    max_tool_rounds: u32,
    turn_id: &TurnId,
    transcript: &[TurnMessage],
    calls: &[KernelToolCall],
    received: EffectId,
    results: Vec<KernelToolResult>,
) -> Result<KernelTransition, KernelInputRejection> {
    let current = KernelPhaseView::ExpectingToolBatchCompletion {
        effect_id: expected.clone(),
        tool_batch_id: batch_id.clone(),
    };
    if received != *expected {
        return mismatch(current, expected, received);
    }
    if !matching_results(calls, &results) {
        return Err(KernelInputRejection {
            phase: current,
            reason: KernelInputRejectionReason::ToolResultsMismatch,
        });
    }
    let mut next_transcript = transcript.to_vec();
    next_transcript.extend(results.iter().cloned().map(TurnMessage::ToolResult));
    let call_number = round + 1;
    let (model_id, model_effect) = model_call_ids(turn_id, call_number);
    let next_state = KernelState {
        inner: State::ExpectingModelCompletion {
            effect_id: model_effect.clone(),
            model_call_id: model_id.clone(),
            used_rounds: round,
            max_tool_rounds,
            turn_id: turn_id.clone(),
            transcript: next_transcript.clone(),
        },
    };
    Ok(KernelTransition {
        phase: phase(&next_state),
        next_state,
        observations: vec![
            KernelObservation::ToolResultsAccepted {
                tool_batch_id: batch_id.clone(),
                results,
            },
            KernelObservation::ModelCallRequested {
                model_call_id: model_id.clone(),
                effect_id: model_effect.clone(),
            },
        ],
        durability: DurabilityRequirement::BeforeNextEffect,
        effect: Some(KernelEffect::RequestModel {
            effect_id: model_effect,
            model_call_id: model_id,
            turn_messages: next_transcript,
            tools_allowed: round < max_tool_rounds,
        }),
    })
}

fn terminate(
    state: &KernelState,
    expected: &EffectId,
    received: EffectId,
    failure: TurnFailure,
) -> Result<KernelTransition, KernelInputRejection> {
    if received != *expected {
        return mismatch(phase(state), expected, received);
    }
    let next_state = KernelState {
        inner: State::Terminated {
            outcome: TurnOutcome::Failed,
            completed_effect_id: expected.clone(),
        },
    };
    Ok(KernelTransition {
        phase: phase(&next_state),
        next_state,
        observations: vec![
            KernelObservation::TurnFailureAccepted {
                effect_id: expected.clone(),
                failure,
            },
            KernelObservation::TurnTerminated {
                outcome: TurnOutcome::Failed,
            },
        ],
        durability: DurabilityRequirement::BeforeSettlement,
        effect: None,
    })
}

fn cancel(
    state: &KernelState,
    expected: &EffectId,
    received: EffectId,
) -> Result<KernelTransition, KernelInputRejection> {
    if received != *expected {
        return mismatch(phase(state), expected, received);
    }
    let next_state = KernelState {
        inner: State::Terminated {
            outcome: TurnOutcome::Cancelled,
            completed_effect_id: expected.clone(),
        },
    };
    Ok(KernelTransition {
        phase: phase(&next_state),
        next_state,
        observations: vec![KernelObservation::TurnTerminated {
            outcome: TurnOutcome::Cancelled,
        }],
        durability: DurabilityRequirement::BeforeSettlement,
        effect: None,
    })
}

fn mismatch<T>(
    phase: KernelPhaseView,
    expected: &EffectId,
    received: EffectId,
) -> Result<T, KernelInputRejection> {
    Err(KernelInputRejection {
        phase,
        reason: KernelInputRejectionReason::EffectIdMismatch {
            expected: expected.clone(),
            received,
        },
    })
}
fn reject<T>(
    state: &KernelState,
    reason: KernelInputRejectionReason,
) -> Result<T, KernelInputRejection> {
    Err(KernelInputRejection {
        phase: phase(state),
        reason,
    })
}
