//! KERNEL-002: 单轮 ToolBatch.

use philo_agent_kernel::*;

fn started() -> KernelTransition {
    transition(
        &initial_state(),
        KernelInput::BeginTurn {
            turn_id: TurnId::new("t"),
            user_message: UserMessage::new("hi"),
            max_tool_rounds: 1,
        },
    )
    .unwrap()
}

#[test]
fn tool_batch_requests_second_model_with_ordered_transcript() {
    let first = started();
    let effect = match first.effect.clone().unwrap() {
        KernelEffect::RequestModel { effect_id, .. } => effect_id,
        _ => unreachable!(),
    };
    let calls = vec![
        KernelToolCall::new(ToolCallId::new("a"), "one", "{}"),
        KernelToolCall::new(ToolCallId::new("b"), "two", "{}"),
    ];
    let tool = transition(
        &first.next_state,
        KernelInput::ModelCallCompleted {
            effect_id: effect,
            output: AssistantOutput::tool_calls(calls.clone()),
        },
    )
    .unwrap();
    assert!(matches!(
        tool.phase,
        KernelPhaseView::ExpectingToolBatchCompletion { .. }
    ));
    let tool_effect = match tool.effect.clone().unwrap() {
        KernelEffect::ExecuteToolBatch { effect_id, .. } => effect_id,
        _ => unreachable!(),
    };
    let results = vec![
        KernelToolResult::success(ToolCallId::new("a"), "1"),
        KernelToolResult::error(ToolCallId::new("b"), "bad", "no"),
    ];
    let second = transition(
        &tool.next_state,
        KernelInput::ToolBatchCompleted {
            effect_id: tool_effect,
            results,
        },
    )
    .unwrap();
    let second_effect = match second.effect.clone().unwrap() {
        KernelEffect::RequestModel {
            effect_id,
            turn_messages,
            ..
        } => {
            assert_eq!(turn_messages.len(), 4);
            effect_id
        }
        _ => unreachable!(),
    };
    let finalised = transition(
        &second.next_state,
        KernelInput::ModelCallCompleted {
            effect_id: second_effect,
            output: AssistantOutput::final_text("done"),
        },
    )
    .unwrap();
    assert_eq!(
        finalised.phase,
        KernelPhaseView::Terminated {
            outcome: TurnOutcome::Succeeded
        }
    );
}

#[test]
fn invalid_tool_results_preserve_state() {
    let first = started();
    let effect = match first.effect.clone().unwrap() {
        KernelEffect::RequestModel { effect_id, .. } => effect_id,
        _ => unreachable!(),
    };
    let call = KernelToolCall::new(ToolCallId::new("a"), "one", "{}");
    let tool = transition(
        &first.next_state,
        KernelInput::ModelCallCompleted {
            effect_id: effect,
            output: AssistantOutput::tool_calls(vec![call]),
        },
    )
    .unwrap();
    let old = tool.next_state.clone();
    assert!(
        transition(
            &old,
            KernelInput::ToolBatchCompleted {
                effect_id: EffectId::new("wrong"),
                results: Vec::new()
            }
        )
        .is_err()
    );
    assert_eq!(old, tool.next_state);
}
