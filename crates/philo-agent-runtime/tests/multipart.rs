//! RUNTIME-007: multi-part `prompt()` input, `ModelMessage::User` parts, and
//! byte-for-byte fidelity across the explicit runtime/kernel/session chain.

mod support;

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use philo_agent_runtime::{
    AgentRuntime, GenerationConfig, InvalidUserMessage, ModelMessage, OperationOutcome,
    RuntimeConfig, SequentialIdSource, SessionId, ToolDefinition, UserMessage, UserPart,
};
use philo_session::{ContextMessage, MemorySessionStore, SessionStore, SessionUserPart};
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};

fn block_on<F: Future>(future: F) -> F::Output {
    let mut context = Context::from_waker(Waker::noop());
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn config(max_tool_rounds: u32) -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "sys".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds,
        max_parallel_tool_calls: 1,
        operation_timeout: None,
        compaction: Default::default(),
    }
}

fn runtime(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<FakeTool>,
    max_tool_rounds: u32,
) -> AgentRuntime {
    AgentRuntime::with_tools(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(max_tool_rounds),
        tools,
    )
}

fn png_bytes() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0xFF, 0x00, 0x7F, 0x42,
    ]
}

fn mixed_message() -> UserMessage {
    UserMessage::from_parts(vec![
        UserPart::Text("what is in this picture?".to_owned()),
        UserPart::Image {
            media_type: "image/png".to_owned(),
            bytes: png_bytes(),
        },
    ])
    .expect("text+image is valid")
}

fn expected_model_parts() -> Vec<UserPart> {
    vec![
        UserPart::Text("what is in this picture?".to_owned()),
        UserPart::Image {
            media_type: "image/png".to_owned(),
            bytes: png_bytes(),
        },
    ]
}

fn expected_session_parts() -> Vec<SessionUserPart> {
    vec![
        SessionUserPart::Text("what is in this picture?".to_owned()),
        SessionUserPart::Image {
            media_type: "image/png".to_owned(),
            bytes: png_bytes(),
        },
    ]
}

// --- M8-001: full fidelity through a multi-round loop --------------------------

#[test]
fn multipart_prompt_completes_a_tool_loop_with_full_fidelity() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::text(&["a cat"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::one(
        ToolDefinition::simple("echo", "echo", philo_agent_runtime::EffectClass::ReadOnly),
        FakeToolResult::success("ok"),
    ));

    let handle = block_on(
        runtime(model.clone(), sessions.clone(), tools, 1)
            .prompt(SessionId::new("s"), mixed_message()),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "a cat"
    ));

    // Both model calls carry the user parts unchanged: the first from the
    // fresh turn, the second from the kernel's self-contained transcript.
    let calls = model.calls();
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert_eq!(
            call.messages[1],
            ModelMessage::User {
                parts: expected_model_parts()
            },
            "user parts replay byte-for-byte in call {}",
            call.model_call_index
        );
    }

    // Barrier A persisted exactly the prompt's parts (runtime -> kernel ->
    // session mapping is lossless).
    let view = block_on(sessions.context_view(&philo_session::SessionId::new("s"))).unwrap();
    assert_eq!(
        view.messages()[0],
        ContextMessage::User {
            parts: expected_session_parts()
        }
    );
}

// --- M8-007: image-only valid, structural rejects ------------------------------

#[test]
fn image_only_prompt_is_valid_and_persists() {
    let message = UserMessage::from_parts(vec![UserPart::Image {
        media_type: "image/jpeg".to_owned(),
        bytes: png_bytes(),
    }])
    .expect("image-only is valid");

    let model = Arc::new(FakeModel::succeeds(&["a dog"]));
    let sessions = Arc::new(MemorySessionStore::new());
    let handle = block_on(
        runtime(
            model.clone(),
            sessions.clone(),
            Arc::new(FakeTool::new([], [])),
            0,
        )
        .prompt(SessionId::new("s"), message),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { .. }
    ));

    assert_eq!(
        model.calls()[0].messages[1],
        ModelMessage::User {
            parts: vec![UserPart::Image {
                media_type: "image/jpeg".to_owned(),
                bytes: png_bytes(),
            }]
        }
    );
    let view = block_on(sessions.context_view(&philo_session::SessionId::new("s"))).unwrap();
    assert_eq!(
        view.messages()[0],
        ContextMessage::User {
            parts: vec![SessionUserPart::Image {
                media_type: "image/jpeg".to_owned(),
                bytes: png_bytes(),
            }]
        }
    );
}

#[test]
fn empty_parts_and_empty_text_are_rejected_at_construction() {
    assert_eq!(
        UserMessage::from_parts(Vec::new()),
        Err(InvalidUserMessage::EmptyParts)
    );
    assert_eq!(
        UserMessage::from_parts(vec![UserPart::Text(String::new())]),
        Err(InvalidUserMessage::EmptyTextPart)
    );
}

// --- Cross-turn context replay ---------------------------------------------------

#[test]
fn image_history_replays_into_the_next_turns_context() {
    let model = Arc::new(FakeModel::succeeds_sequence(vec![
        vec!["a cat"],
        vec!["still a cat"],
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let agent = runtime(model.clone(), sessions, Arc::new(FakeTool::new([], [])), 0);

    let first = block_on(agent.prompt(SessionId::new("s"), mixed_message())).unwrap();
    assert!(matches!(
        block_on(first.wait()),
        OperationOutcome::Succeeded { .. }
    ));
    let second = block_on(agent.prompt(SessionId::new("s"), UserMessage::new("and now?"))).unwrap();
    assert!(matches!(
        block_on(second.wait()),
        OperationOutcome::Succeeded { .. }
    ));

    let calls = model.calls();
    assert_eq!(calls.len(), 2);
    // The second turn's context replays the first turn's image parts from
    // the session, byte-for-byte, before the new plain-text message.
    assert_eq!(
        calls[1].messages[1],
        ModelMessage::User {
            parts: expected_model_parts()
        }
    );
    assert_eq!(
        calls[1].messages[3],
        ModelMessage::User {
            parts: vec![UserPart::Text("and now?".to_owned())]
        }
    );
}
