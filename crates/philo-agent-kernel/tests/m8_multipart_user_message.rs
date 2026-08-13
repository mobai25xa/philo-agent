use philo_agent_kernel::*;

fn png_bytes() -> Vec<u8> {
    vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x01]
}

fn image_part() -> UserPart {
    UserPart::Image {
        media_type: "image/png".to_owned(),
        bytes: png_bytes(),
    }
}

fn mixed_message() -> UserMessage {
    UserMessage::from_parts(vec![
        UserPart::Text("what is in this picture?".to_owned()),
        image_part(),
    ])
    .expect("text+image message is valid")
}

fn begin(user: UserMessage) -> KernelTransition {
    transition(
        &initial_state(),
        KernelInput::BeginTurn {
            turn_id: TurnId::new("t"),
            user_message: user,
            max_tool_rounds: 1,
        },
    )
    .expect("turn should begin")
}

#[test]
fn multipart_message_flows_into_observation_and_effect() {
    let started = begin(mixed_message());

    assert_eq!(
        started.observations[0],
        KernelObservation::TurnBegan {
            turn_id: TurnId::new("t"),
            user_message: mixed_message(),
        }
    );
    let KernelEffect::RequestModel { turn_messages, .. } = started.effect.expect("model effect")
    else {
        panic!("expected model effect")
    };
    assert_eq!(turn_messages, vec![TurnMessage::User(mixed_message())]);
}

#[test]
fn multipart_message_stays_self_contained_across_tool_rounds() {
    let started = begin(mixed_message());
    let KernelEffect::RequestModel { effect_id, .. } = started.effect.clone().unwrap() else {
        panic!("expected model effect")
    };

    let call = KernelToolCall::new(ToolCallId::new("a"), "read", "{}");
    let tool = transition(
        &started.next_state,
        KernelInput::ModelCallCompleted {
            effect_id,
            output: AssistantOutput::tool_calls(vec![call]),
        },
    )
    .unwrap();
    let KernelEffect::ExecuteToolBatch { effect_id, .. } = tool.effect.clone().unwrap() else {
        panic!("expected tool effect")
    };

    let second = transition(
        &tool.next_state,
        KernelInput::ToolBatchCompleted {
            effect_id,
            results: vec![KernelToolResult::success(ToolCallId::new("a"), "ok")],
        },
    )
    .unwrap();
    let KernelEffect::RequestModel { turn_messages, .. } = second.effect.unwrap() else {
        panic!("expected second model effect")
    };
    // The follow-up request stays self-contained: the multi-part user
    // message is replayed byte-for-byte at the head of the transcript.
    assert_eq!(turn_messages.len(), 3);
    assert_eq!(turn_messages[0], TurnMessage::User(mixed_message()));
}

#[test]
fn image_only_message_is_valid() {
    let message = UserMessage::from_parts(vec![image_part()]).expect("image-only is valid");
    let started = begin(message.clone());
    let KernelEffect::RequestModel { turn_messages, .. } = started.effect.unwrap() else {
        panic!("expected model effect")
    };
    assert_eq!(turn_messages, vec![TurnMessage::User(message)]);
}

#[test]
fn empty_parts_are_rejected() {
    assert_eq!(
        UserMessage::from_parts(Vec::new()),
        Err(InvalidUserMessage::EmptyParts)
    );
}

#[test]
fn empty_text_part_is_rejected() {
    assert_eq!(
        UserMessage::from_parts(vec![UserPart::Text(String::new()), image_part()]),
        Err(InvalidUserMessage::EmptyTextPart)
    );
}

#[test]
#[should_panic(expected = "must not be empty")]
fn plain_text_convenience_constructor_rejects_empty_text() {
    let _ = UserMessage::new("");
}

#[test]
fn plain_text_convenience_constructor_is_a_single_text_part() {
    assert_eq!(
        UserMessage::new("hi").parts(),
        &[UserPart::Text("hi".to_owned())]
    );
}

#[test]
fn transition_with_image_input_is_deterministic() {
    let input = KernelInput::BeginTurn {
        turn_id: TurnId::new("stable-turn"),
        user_message: mixed_message(),
        max_tool_rounds: 1,
    };
    let state = initial_state();
    assert_eq!(transition(&state, input.clone()), transition(&state, input));
}
