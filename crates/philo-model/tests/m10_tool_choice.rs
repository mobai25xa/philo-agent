//! MODEL-005: tool_choice direct mapping, Specific pre-send validation, and
//! the parallel_tool_calls Forbid lock (M10-008).

mod support;

use philo_agent_runtime::{
    GenerationConfig, ModelCallId, ModelCallSnapshot, ModelMessage, ModelPort, OperationId,
    ToolChoice, TurnId, UserPart,
};
use support::{
    StubResponse, StubTransport, adapter_over, collect_ok, read_tool_definition, text_sse,
};

fn snapshot_with_choice(
    tool_choice: ToolChoice,
    tools: Vec<philo_agent_runtime::ToolDefinition>,
) -> ModelCallSnapshot {
    ModelCallSnapshot {
        session_id: philo_agent_runtime::SessionId::new("session-1"),
        context_fingerprint: "session-1:entry:1".to_owned(),
        persist_replay: true,
        operation_id: OperationId::new("operation-1"),
        turn_id: TurnId::new("turn-1"),
        model_call_id: ModelCallId::new("model-call-1"),
        model_call_index: 1,
        session_revision: philo_session::SessionRevision::ZERO,
        messages: vec![ModelMessage::User {
            parts: vec![UserPart::Text("hi".to_owned())],
        }],
        tools,
        model_target: "stub-model".to_owned(),
        generation: GenerationConfig {
            max_output_tokens: 256,
            temperature: 0.25,
            reasoning_effort: None,
            tool_choice,
        },
    }
}

#[tokio::test]
async fn default_auto_keeps_the_prior_request_shape() {
    let transport = StubTransport::new([StubResponse::Sse(text_sse("r", "m", &["ok"]))]);
    let adapter = adapter_over(transport.clone());
    let stream = adapter
        .start(snapshot_with_choice(
            ToolChoice::Auto,
            vec![read_tool_definition()],
        ))
        .await
        .expect("call starts");
    collect_ok(stream).await;

    let body = &transport.request_bodies()[0];
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(
        body["parallel_tool_calls"], false,
        "the kernel serial invariant locks parallel off"
    );
}

#[tokio::test]
async fn required_none_and_specific_map_directly() {
    let transport = StubTransport::new([
        StubResponse::Sse(text_sse("r1", "m", &["a"])),
        StubResponse::Sse(text_sse("r2", "m", &["b"])),
        StubResponse::Sse(text_sse("r3", "m", &["c"])),
    ]);
    let adapter = adapter_over(transport.clone());

    for choice in [
        ToolChoice::Required,
        ToolChoice::None,
        ToolChoice::Specific {
            name: "read".to_owned(),
        },
    ] {
        let stream = adapter
            .start(snapshot_with_choice(choice, vec![read_tool_definition()]))
            .await
            .expect("call starts");
        collect_ok(stream).await;
    }

    let bodies = transport.request_bodies();
    assert_eq!(bodies[0]["tool_choice"], "required");
    assert_eq!(bodies[1]["tool_choice"], "none");
    assert_eq!(bodies[2]["tool_choice"]["type"], "function");
    assert_eq!(bodies[2]["tool_choice"]["function"]["name"], "read");
    for body in &bodies {
        assert_eq!(body["parallel_tool_calls"], false);
    }
}

#[tokio::test]
async fn tool_disabled_calls_ignore_the_configuration() {
    let transport = StubTransport::new([StubResponse::Sse(text_sse("r", "m", &["ok"]))]);
    let adapter = adapter_over(transport.clone());
    // Required configured, but the call carries no tools (tools_allowed =
    // false): tool disabling wins.
    let stream = adapter
        .start(snapshot_with_choice(ToolChoice::Required, Vec::new()))
        .await
        .expect("call starts");
    collect_ok(stream).await;

    let body = &transport.request_bodies()[0];
    assert!(
        body.get("tool_choice").is_none(),
        "no tools: the wire omits tool_choice entirely (None semantics)"
    );
    assert!(body.get("tools").is_none());
}

#[tokio::test]
async fn specific_with_an_unknown_name_fails_before_any_transport_call() {
    let transport = StubTransport::new([StubResponse::Sse(text_sse("r", "m", &["ok"]))]);
    let adapter = adapter_over(transport.clone());
    let Err(error) = adapter
        .start(snapshot_with_choice(
            ToolChoice::Specific {
                name: "ghost".to_owned(),
            },
            vec![read_tool_definition()],
        ))
        .await
    else {
        panic!("an unknown Specific name must be rejected");
    };
    assert!(
        error.message().contains("ghost"),
        "diagnostic names the missing tool: {}",
        error.message()
    );
    assert!(
        transport.requests().is_empty(),
        "validation happens before any transport call"
    );
}
