use philo_agent_kernel::{
    AssistantOutput, DurabilityRequirement, EffectId, KernelEffect, KernelInput,
    KernelInputRejectionReason, KernelObservation, KernelPhaseView, TurnFailure, TurnId,
    TurnMessage, TurnOutcome, UserMessage, initial_state, phase, transition,
};

fn begin() -> (
    philo_agent_kernel::KernelTransition,
    EffectId,
    philo_agent_kernel::ModelCallId,
) {
    let started = transition(
        &initial_state(),
        KernelInput::BeginTurn {
            turn_id: TurnId::new("turn-1"),
            user_message: UserMessage::new("hi"),
            max_tool_rounds: 1,
        },
    )
    .expect("turn should begin");
    let KernelEffect::RequestModel {
        effect_id,
        model_call_id,
        turn_messages,
        ..
    } = started.effect.clone().expect("model effect")
    else {
        panic!("expected model effect")
    };
    assert_eq!(
        turn_messages,
        vec![TurnMessage::User(UserMessage::new("hi"))]
    );
    (started, effect_id, model_call_id)
}

#[test]
fn kernel_m1_001_direct_answer_success() {
    assert_eq!(phase(&initial_state()), KernelPhaseView::ExpectingTurnStart);
    let (started, effect_id, model_call_id) = begin();
    assert_eq!(started.durability, DurabilityRequirement::BeforeNextEffect);
    assert_eq!(
        started.phase,
        KernelPhaseView::ExpectingModelCompletion {
            effect_id: effect_id.clone(),
            model_call_id: model_call_id.clone(),
        }
    );

    let completed = transition(
        &started.next_state,
        KernelInput::ModelCallCompleted {
            effect_id,
            output: AssistantOutput::final_text("hello"),
        },
    )
    .expect("model call should complete");

    assert_eq!(
        completed.phase,
        KernelPhaseView::Terminated {
            outcome: TurnOutcome::Succeeded
        }
    );
    assert_eq!(
        completed.durability,
        DurabilityRequirement::BeforeSettlement
    );
    assert!(completed.effect.is_none());
    assert_eq!(
        completed.observations,
        vec![
            KernelObservation::AssistantOutputAccepted {
                model_call_id,
                output: AssistantOutput::final_text("hello"),
            },
            KernelObservation::TurnTerminated {
                outcome: TurnOutcome::Succeeded,
            },
        ]
    );
}

#[test]
fn kernel_m1_002_duplicate_begin_turn_rejected() {
    let (started, _, _) = begin();
    let rejection = transition(
        &started.next_state,
        KernelInput::BeginTurn {
            turn_id: TurnId::new("turn-2"),
            user_message: UserMessage::new("again"),
            max_tool_rounds: 1,
        },
    )
    .expect_err("duplicate start must fail");
    assert_eq!(
        rejection.reason(),
        &KernelInputRejectionReason::InputNotAccepted
    );
}

#[test]
fn kernel_m1_003_wrong_effect_id_rejected() {
    let (started, expected, model_call_id) = begin();
    let received = EffectId::new("wrong");
    let rejection = transition(
        &started.next_state,
        KernelInput::ModelCallCompleted {
            effect_id: received.clone(),
            output: AssistantOutput::final_text("hello"),
        },
    )
    .expect_err("wrong effect must fail");
    assert_eq!(
        rejection.phase(),
        &KernelPhaseView::ExpectingModelCompletion {
            effect_id: expected.clone(),
            model_call_id,
        }
    );
    assert_eq!(
        rejection.reason(),
        &KernelInputRejectionReason::EffectIdMismatch { expected, received }
    );
}

#[test]
fn kernel_m1_004_unsupported_tool_call_output_rejected() {
    let (started, effect_id, _) = begin();
    let rejection = transition(
        &started.next_state,
        KernelInput::ModelCallCompleted {
            effect_id,
            output: AssistantOutput::with_unsupported_tool_call("calling tool"),
        },
    )
    .expect_err("tool calls are outside M1");
    assert_eq!(
        rejection.reason(),
        &KernelInputRejectionReason::UnsupportedAssistantOutput
    );
}

#[test]
fn kernel_m1_005_terminal_input_rejected() {
    let (started, effect_id, _) = begin();
    let completed = transition(
        &started.next_state,
        KernelInput::ModelCallCompleted {
            effect_id: effect_id.clone(),
            output: AssistantOutput::final_text("hello"),
        },
    )
    .unwrap();
    let rejection = transition(
        &completed.next_state,
        KernelInput::BeginTurn {
            turn_id: TurnId::new("turn-2"),
            user_message: UserMessage::new("again"),
            max_tool_rounds: 1,
        },
    )
    .expect_err("terminal states cannot advance");
    assert_eq!(
        rejection.reason(),
        &KernelInputRejectionReason::KernelTerminated
    );

    let duplicate = transition(
        &completed.next_state,
        KernelInput::ModelCallCompleted {
            effect_id: effect_id.clone(),
            output: AssistantOutput::final_text("again"),
        },
    )
    .expect_err("completed effects cannot complete twice");
    assert_eq!(
        duplicate.reason(),
        &KernelInputRejectionReason::EffectAlreadyCompleted { effect_id }
    );
}

#[test]
fn kernel_m1_006_rejected_input_preserves_old_state() {
    let (started, _, _) = begin();
    let old_state = started.next_state.clone();
    let _ = transition(
        &old_state,
        KernelInput::ModelCallCompleted {
            effect_id: EffectId::new("wrong"),
            output: AssistantOutput::final_text("hello"),
        },
    );
    assert_eq!(old_state, started.next_state);
}

#[test]
fn kernel_m1_007_transition_is_deterministic() {
    let state = initial_state();
    let input = KernelInput::BeginTurn {
        turn_id: TurnId::new("stable-turn"),
        user_message: UserMessage::new("same"),
        max_tool_rounds: 1,
    };
    assert_eq!(transition(&state, input.clone()), transition(&state, input));
}

#[test]
fn kernel_m1_008_matching_termination_request_fails_turn() {
    let (started, effect_id, _) = begin();
    let failure = TurnFailure::ModelCallFailed {
        message: "offline".to_owned(),
    };
    let terminated = transition(
        &started.next_state,
        KernelInput::TerminationRequested {
            effect_id: effect_id.clone(),
            failure: failure.clone(),
        },
    )
    .expect("matching termination should be accepted");
    assert_eq!(
        terminated.phase,
        KernelPhaseView::Terminated {
            outcome: TurnOutcome::Failed
        }
    );
    assert_eq!(
        terminated.durability,
        DurabilityRequirement::BeforeSettlement
    );
    assert_eq!(
        terminated.observations,
        vec![
            KernelObservation::TurnFailureAccepted { effect_id, failure },
            KernelObservation::TurnTerminated {
                outcome: TurnOutcome::Failed,
            },
        ]
    );
}

#[test]
fn kernel_m1_009_wrong_effect_id_termination_rejected() {
    let (started, expected, _) = begin();
    let received = EffectId::new("wrong");
    let rejection = transition(
        &started.next_state,
        KernelInput::TerminationRequested {
            effect_id: received.clone(),
            failure: TurnFailure::RuntimeDriverFailed {
                message: "driver".to_owned(),
            },
        },
    )
    .expect_err("wrong termination effect must fail");
    assert_eq!(
        rejection.reason(),
        &KernelInputRejectionReason::EffectIdMismatch { expected, received }
    );
}
