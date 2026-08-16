//! KERNEL-003: 多轮与 `max_tool_rounds`.

use philo_agent_kernel::*;

fn begin_with(max_tool_rounds: u32) -> KernelTransition {
    transition(
        &initial_state(),
        KernelInput::BeginTurn {
            turn_id: TurnId::new("t"),
            user_message: UserMessage::new("hi"),
            max_tool_rounds,
        },
    )
    .unwrap()
}

fn model_effect(transition: &KernelTransition) -> (EffectId, bool, Vec<TurnMessage>) {
    match transition.effect.clone().unwrap() {
        KernelEffect::RequestModel {
            effect_id,
            tools_allowed,
            turn_messages,
            ..
        } => (effect_id, tools_allowed, turn_messages),
        other => panic!("expected model effect, got {other:?}"),
    }
}

fn tool_effect(transition: &KernelTransition) -> (EffectId, ToolBatchId) {
    match transition.effect.clone().unwrap() {
        KernelEffect::ExecuteToolBatch {
            effect_id,
            tool_batch_id,
            ..
        } => (effect_id, tool_batch_id),
        other => panic!("expected tool effect, got {other:?}"),
    }
}

fn single_call(id: &str) -> Vec<KernelToolCall> {
    vec![KernelToolCall::new(ToolCallId::new(id), "tool", "{}")]
}

fn success_result(id: &str) -> Vec<KernelToolResult> {
    vec![KernelToolResult::success(ToolCallId::new(id), "ok")]
}

/// Drives one complete tool round; returns the transition awaiting the next model completion.
fn run_round(state: &KernelState, model_effect_id: EffectId, call_id: &str) -> KernelTransition {
    let batch = transition(
        state,
        KernelInput::ModelCallCompleted {
            effect_id: model_effect_id,
            output: AssistantOutput::tool_calls(single_call(call_id)),
        },
    )
    .unwrap();
    let (tool_effect_id, _) = tool_effect(&batch);
    transition(
        &batch.next_state,
        KernelInput::ToolBatchCompleted {
            effect_id: tool_effect_id,
            results: success_result(call_id),
        },
    )
    .unwrap()
}

#[test]
fn zero_rounds_never_allows_tools() {
    let started = begin_with(0);
    let (effect_id, tools_allowed, _) = model_effect(&started);
    assert!(!tools_allowed);

    let rejection = transition(
        &started.next_state,
        KernelInput::ModelCallCompleted {
            effect_id: effect_id.clone(),
            output: AssistantOutput::tool_calls(single_call("a")),
        },
    )
    .expect_err("tools are disabled for max_tool_rounds = 0");
    assert_eq!(
        rejection.reason(),
        &KernelInputRejectionReason::UnsupportedAssistantOutput
    );

    let done = transition(
        &started.next_state,
        KernelInput::ModelCallCompleted {
            effect_id,
            output: AssistantOutput::final_text("plain answer"),
        },
    )
    .unwrap();
    assert_eq!(
        done.phase,
        KernelPhaseView::Terminated {
            outcome: TurnOutcome::Succeeded
        }
    );
}

#[test]
fn single_round_matches_the_single_tool_loop_trace() {
    let started = begin_with(1);
    let (first_effect, tools_allowed, _) = model_effect(&started);
    assert!(tools_allowed);
    assert_eq!(first_effect.as_str(), "t:effect:1");

    let batch = transition(
        &started.next_state,
        KernelInput::ModelCallCompleted {
            effect_id: first_effect,
            output: AssistantOutput::tool_calls(single_call("a")),
        },
    )
    .unwrap();
    let (tool_effect_id, batch_id) = tool_effect(&batch);
    assert_eq!(tool_effect_id.as_str(), "t:effect:2");
    assert_eq!(batch_id.as_str(), "t:tool-batch:1");

    let second = transition(
        &batch.next_state,
        KernelInput::ToolBatchCompleted {
            effect_id: tool_effect_id,
            results: success_result("a"),
        },
    )
    .unwrap();
    let (second_effect, second_allowed, transcript) = model_effect(&second);
    assert_eq!(second_effect.as_str(), "t:effect:3");
    assert!(!second_allowed, "round budget of one is exhausted");
    assert_eq!(transcript.len(), 3);
}

#[test]
fn two_rounds_accumulate_transcript_and_monotonic_ids() {
    let started = begin_with(2);
    let (first_effect, tools_allowed, _) = model_effect(&started);
    assert!(tools_allowed);

    let second = run_round(&started.next_state, first_effect, "a");
    let (second_effect, second_allowed, second_transcript) = model_effect(&second);
    assert_eq!(second_effect.as_str(), "t:effect:3");
    assert!(second_allowed, "one round left of two");
    assert_eq!(second_transcript.len(), 3);

    let batch2 = transition(
        &second.next_state,
        KernelInput::ModelCallCompleted {
            effect_id: second_effect,
            output: AssistantOutput::tool_calls(single_call("b")),
        },
    )
    .unwrap();
    let (tool_effect_2, batch_id_2) = tool_effect(&batch2);
    assert_eq!(tool_effect_2.as_str(), "t:effect:4");
    assert_eq!(batch_id_2.as_str(), "t:tool-batch:2");

    let third = transition(
        &batch2.next_state,
        KernelInput::ToolBatchCompleted {
            effect_id: tool_effect_2,
            results: success_result("b"),
        },
    )
    .unwrap();
    let (third_effect, third_allowed, third_transcript) = model_effect(&third);
    assert_eq!(third_effect.as_str(), "t:effect:5");
    assert!(!third_allowed, "both rounds are used");
    assert_eq!(
        third_transcript.len(),
        5,
        "user + two batches + two results stay in source order"
    );
    assert!(matches!(third_transcript[0], TurnMessage::User(_)));
    assert!(matches!(
        third_transcript[1],
        TurnMessage::AssistantToolCalls { .. }
    ));
    assert!(matches!(third_transcript[2], TurnMessage::ToolResult(_)));
    assert!(matches!(
        third_transcript[3],
        TurnMessage::AssistantToolCalls { .. }
    ));
    assert!(matches!(third_transcript[4], TurnMessage::ToolResult(_)));

    let done = transition(
        &third.next_state,
        KernelInput::ModelCallCompleted {
            effect_id: third_effect,
            output: AssistantOutput::final_text("final"),
        },
    )
    .unwrap();
    assert_eq!(
        done.phase,
        KernelPhaseView::Terminated {
            outcome: TurnOutcome::Succeeded
        }
    );
}

#[test]
fn exhausted_call_rejects_tool_calls_and_preserves_state() {
    let started = begin_with(1);
    let (first_effect, _, _) = model_effect(&started);
    let second = run_round(&started.next_state, first_effect, "a");
    let (second_effect, second_allowed, _) = model_effect(&second);
    assert!(!second_allowed);

    let old_state = second.next_state.clone();
    let rejection = transition(
        &old_state,
        KernelInput::ModelCallCompleted {
            effect_id: second_effect.clone(),
            output: AssistantOutput::tool_calls(single_call("b")),
        },
    )
    .expect_err("tool calls after exhaustion must be rejected");
    assert_eq!(
        rejection.reason(),
        &KernelInputRejectionReason::UnsupportedAssistantOutput
    );
    assert_eq!(old_state, second.next_state, "rejection keeps the state");

    let failed = transition(
        &old_state,
        KernelInput::TerminationRequested {
            effect_id: second_effect,
            failure: TurnFailure::InvalidModelOutput {
                message: "tool calls while tools are disabled".to_owned(),
            },
        },
    )
    .unwrap();
    assert_eq!(
        failed.phase,
        KernelPhaseView::Terminated {
            outcome: TurnOutcome::Failed
        }
    );
    assert_eq!(failed.durability, DurabilityRequirement::BeforeSettlement);
}

#[test]
fn tool_error_does_not_consume_extra_round() {
    let started = begin_with(2);
    let (first_effect, _, _) = model_effect(&started);

    let batch = transition(
        &started.next_state,
        KernelInput::ModelCallCompleted {
            effect_id: first_effect,
            output: AssistantOutput::tool_calls(single_call("a")),
        },
    )
    .unwrap();
    let (tool_effect_id, _) = tool_effect(&batch);
    let second = transition(
        &batch.next_state,
        KernelInput::ToolBatchCompleted {
            effect_id: tool_effect_id,
            results: vec![KernelToolResult::error(
                ToolCallId::new("a"),
                "unknown_tool",
                "no such tool",
            )],
        },
    )
    .unwrap();
    let (_, tools_allowed, _) = model_effect(&second);
    assert!(
        tools_allowed,
        "an error result uses exactly one round, same as success"
    );
}

#[test]
fn transition_is_deterministic_across_rounds() {
    let started = begin_with(3);
    let (first_effect, _, _) = model_effect(&started);
    let second = run_round(&started.next_state, first_effect, "a");
    let (second_effect, _, _) = model_effect(&second);

    let input = KernelInput::ModelCallCompleted {
        effect_id: second_effect,
        output: AssistantOutput::tool_calls(single_call("b")),
    };
    assert_eq!(
        transition(&second.next_state, input.clone()),
        transition(&second.next_state, input)
    );
}
