//! KERNEL-004: `CancelRequested` input and the `Cancelled` terminal outcome.

use philo_agent_kernel::*;

fn begun() -> (KernelTransition, EffectId) {
    let started = transition(
        &initial_state(),
        KernelInput::BeginTurn {
            turn_id: TurnId::new("turn-1"),
            user_message: UserMessage::new("hi"),
            max_tool_rounds: 2,
        },
    )
    .expect("turn should begin");
    let KernelEffect::RequestModel { effect_id, .. } = started.effect.clone().expect("effect")
    else {
        panic!("expected model effect")
    };
    (started, effect_id)
}

/// Advances the begun turn into `ExpectingToolBatchCompletion`.
fn in_tool_batch() -> (KernelTransition, EffectId) {
    let (started, model_effect) = begun();
    let calls = vec![KernelToolCall::new(ToolCallId::new("call-a"), "read", "{}")];
    let batch = transition(
        &started.next_state,
        KernelInput::ModelCallCompleted {
            effect_id: model_effect,
            output: AssistantOutput::tool_calls(calls),
        },
    )
    .expect("tool batch should be requested");
    let KernelEffect::ExecuteToolBatch { effect_id, .. } = batch.effect.clone().expect("effect")
    else {
        panic!("expected tool effect")
    };
    (batch, effect_id)
}

fn assert_cancelled(transition_result: &KernelTransition) {
    assert_eq!(
        transition_result.phase,
        KernelPhaseView::Terminated {
            outcome: TurnOutcome::Cancelled
        }
    );
    assert_eq!(
        transition_result.durability,
        DurabilityRequirement::BeforeSettlement
    );
    assert!(transition_result.effect.is_none());
    assert_eq!(
        transition_result.observations,
        vec![KernelObservation::TurnTerminated {
            outcome: TurnOutcome::Cancelled,
        }]
    );
}

#[test]
fn cancel_during_model_completion_terminates_cancelled() {
    let (started, effect_id) = begun();
    let cancelled = transition(
        &started.next_state,
        KernelInput::CancelRequested { effect_id },
    )
    .expect("matching cancel should be accepted");
    assert_cancelled(&cancelled);
}

#[test]
fn cancel_during_tool_batch_terminates_cancelled() {
    let (batch, effect_id) = in_tool_batch();
    let cancelled = transition(
        &batch.next_state,
        KernelInput::CancelRequested { effect_id },
    )
    .expect("matching cancel should be accepted");
    assert_cancelled(&cancelled);
}

#[test]
fn cancel_before_turn_start_is_rejected() {
    let state = initial_state();
    let rejection = transition(
        &state,
        KernelInput::CancelRequested {
            effect_id: EffectId::new("anything"),
        },
    )
    .expect_err("no outstanding effect to cancel");
    assert_eq!(
        rejection.reason(),
        &KernelInputRejectionReason::InputNotAccepted
    );
    assert_eq!(rejection.phase(), &KernelPhaseView::ExpectingTurnStart);
}

#[test]
fn cancel_with_mismatched_effect_id_is_rejected_and_state_unchanged() {
    let (started, expected) = begun();
    let old_state = started.next_state.clone();
    let received = EffectId::new("wrong");
    let rejection = transition(
        &old_state,
        KernelInput::CancelRequested {
            effect_id: received.clone(),
        },
    )
    .expect_err("mismatched cancel must fail");
    assert_eq!(
        rejection.reason(),
        &KernelInputRejectionReason::EffectIdMismatch { expected, received }
    );
    assert_eq!(old_state, started.next_state);
}

#[test]
fn rejected_cancel_leaves_the_original_effect_completable() {
    let (started, effect_id) = begun();
    let _ = transition(
        &started.next_state,
        KernelInput::CancelRequested {
            effect_id: EffectId::new("wrong"),
        },
    );
    let completed = transition(
        &started.next_state,
        KernelInput::ModelCallCompleted {
            effect_id,
            output: AssistantOutput::final_text("hello"),
        },
    )
    .expect("original effect should still complete");
    assert_eq!(
        completed.phase,
        KernelPhaseView::Terminated {
            outcome: TurnOutcome::Succeeded
        }
    );
}

#[test]
fn cancel_after_termination_is_rejected() {
    let (started, effect_id) = begun();
    let succeeded = transition(
        &started.next_state,
        KernelInput::ModelCallCompleted {
            effect_id: effect_id.clone(),
            output: AssistantOutput::final_text("done"),
        },
    )
    .unwrap();
    let rejection = transition(
        &succeeded.next_state,
        KernelInput::CancelRequested { effect_id },
    )
    .expect_err("terminated kernel rejects cancellation");
    assert_eq!(
        rejection.reason(),
        &KernelInputRejectionReason::KernelTerminated
    );
}

#[test]
fn cancelled_kernel_rejects_every_advancing_input() {
    let (started, effect_id) = begun();
    let cancelled = transition(
        &started.next_state,
        KernelInput::CancelRequested {
            effect_id: effect_id.clone(),
        },
    )
    .unwrap();

    let begin_again = transition(
        &cancelled.next_state,
        KernelInput::BeginTurn {
            turn_id: TurnId::new("turn-2"),
            user_message: UserMessage::new("again"),
            max_tool_rounds: 1,
        },
    )
    .expect_err("cancelled kernel cannot start a turn");
    assert_eq!(
        begin_again.reason(),
        &KernelInputRejectionReason::KernelTerminated
    );

    let late_completion = transition(
        &cancelled.next_state,
        KernelInput::ModelCallCompleted {
            effect_id: effect_id.clone(),
            output: AssistantOutput::final_text("late"),
        },
    )
    .expect_err("cancelled effect cannot complete afterwards");
    assert_eq!(
        late_completion.reason(),
        &KernelInputRejectionReason::EffectAlreadyCompleted {
            effect_id: effect_id.clone()
        }
    );

    let repeat_cancel = transition(
        &cancelled.next_state,
        KernelInput::CancelRequested { effect_id },
    )
    .expect_err("cancel after termination is rejected");
    assert_eq!(
        repeat_cancel.reason(),
        &KernelInputRejectionReason::KernelTerminated
    );

    let termination = transition(
        &cancelled.next_state,
        KernelInput::TerminationRequested {
            effect_id: EffectId::new("other"),
            failure: TurnFailure::RuntimeDriverFailed {
                message: "late".to_owned(),
            },
        },
    )
    .expect_err("cancelled kernel rejects termination requests");
    assert_eq!(
        termination.reason(),
        &KernelInputRejectionReason::KernelTerminated
    );
}

#[test]
fn cancel_transition_is_deterministic() {
    let (started, effect_id) = begun();
    let input = KernelInput::CancelRequested { effect_id };
    assert_eq!(
        transition(&started.next_state, input.clone()),
        transition(&started.next_state, input)
    );
}

#[test]
fn cancel_during_tool_batch_after_completed_rounds_is_deterministic() {
    let (batch, tool_effect) = in_tool_batch();
    let results = vec![KernelToolResult::success(ToolCallId::new("call-a"), "ok")];
    let second_model = transition(
        &batch.next_state,
        KernelInput::ToolBatchCompleted {
            effect_id: tool_effect,
            results,
        },
    )
    .expect("first round completes");
    let KernelEffect::RequestModel { effect_id, .. } =
        second_model.effect.clone().expect("second model call")
    else {
        panic!("expected model effect")
    };
    let cancelled = transition(
        &second_model.next_state,
        KernelInput::CancelRequested { effect_id },
    )
    .expect("cancel in a later round is accepted");
    assert_cancelled(&cancelled);
}
