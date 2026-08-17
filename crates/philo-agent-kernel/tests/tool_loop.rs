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

#[test]
fn mixed_text_and_tool_call_blocks_are_accepted_in_source_order() {
    let first = started();
    let effect = match first.effect.clone().unwrap() {
        KernelEffect::RequestModel {
            effect_id,
            tools_allowed,
            ..
        } => {
            assert!(tools_allowed);
            effect_id
        }
        _ => unreachable!(),
    };
    let call = KernelToolCall::new(ToolCallId::new("a"), "one", "{}");
    let blocks = vec![
        AssistantBlock::Text {
            text: "preamble".to_owned(),
        },
        AssistantBlock::ToolCall(call.clone()),
    ];
    let output = AssistantOutput::from_blocks(blocks.clone()).unwrap();
    assert_eq!(output.text(), "preamble");
    assert_eq!(output.tool_call_batch(), Some(vec![call.clone()]));

    let tool = transition(
        &first.next_state,
        KernelInput::ModelCallCompleted {
            effect_id: effect,
            output,
        },
    )
    .expect("text + tool-call blocks are accepted when tools are allowed");

    match tool.effect.clone().unwrap() {
        KernelEffect::ExecuteToolBatch { calls, .. } => {
            assert_eq!(calls, vec![call.clone()]);
        }
        other => panic!("expected tool effect, got {other:?}"),
    }
    match &tool.observations[0] {
        KernelObservation::AssistantToolCallsAccepted {
            blocks: observed_blocks,
            calls,
            ..
        } => {
            assert_eq!(observed_blocks, &blocks);
            assert_eq!(calls, &vec![call.clone()]);
        }
        other => panic!("expected tool-call observation, got {other:?}"),
    }

    let tool_effect = match tool.effect.clone().unwrap() {
        KernelEffect::ExecuteToolBatch { effect_id, .. } => effect_id,
        _ => unreachable!(),
    };
    let second = transition(
        &tool.next_state,
        KernelInput::ToolBatchCompleted {
            effect_id: tool_effect,
            results: vec![KernelToolResult::success(ToolCallId::new("a"), "1")],
        },
    )
    .unwrap();
    match second.effect.unwrap() {
        KernelEffect::RequestModel { turn_messages, .. } => match &turn_messages[1] {
            TurnMessage::AssistantToolCalls {
                blocks: transcript_blocks,
                ..
            } => assert_eq!(transcript_blocks, &blocks),
            other => panic!("expected assistant tool-call message, got {other:?}"),
        },
        other => panic!("expected follow-up model effect, got {other:?}"),
    }
}

#[test]
fn mixed_blocks_rejected_when_tools_disabled() {
    let started = transition(
        &initial_state(),
        KernelInput::BeginTurn {
            turn_id: TurnId::new("t"),
            user_message: UserMessage::new("hi"),
            max_tool_rounds: 0,
        },
    )
    .unwrap();
    let effect = match started.effect.clone().unwrap() {
        KernelEffect::RequestModel {
            effect_id,
            tools_allowed,
            ..
        } => {
            assert!(!tools_allowed);
            effect_id
        }
        _ => unreachable!(),
    };
    let output = AssistantOutput::from_blocks(vec![
        AssistantBlock::Text {
            text: "preamble".to_owned(),
        },
        AssistantBlock::ToolCall(KernelToolCall::new(ToolCallId::new("a"), "one", "{}")),
    ])
    .unwrap();
    let old = started.next_state.clone();
    let rejection = transition(
        &old,
        KernelInput::ModelCallCompleted {
            effect_id: effect,
            output,
        },
    )
    .expect_err("a ToolCall stays unsupported when tools are disabled");
    assert_eq!(
        rejection.reason(),
        &KernelInputRejectionReason::UnsupportedAssistantOutput
    );
    assert_eq!(old, started.next_state);
}

#[test]
fn from_blocks_rejects_empty_text_block() {
    assert_eq!(
        AssistantOutput::from_blocks(vec![AssistantBlock::Text {
            text: String::new()
        }]),
        Err(InvalidAssistantOutput::EmptyTextBlock)
    );
}

#[test]
fn final_text_empty_string_yields_empty_blocks() {
    let output = AssistantOutput::final_text("");
    assert!(output.blocks().is_empty());
    assert_eq!(output, AssistantOutput::from_blocks(Vec::new()).unwrap());
}
