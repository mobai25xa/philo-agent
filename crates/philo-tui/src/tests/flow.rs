//! Fake-host complete interaction: session selection, history, confirmation,
//! and queueing through the same pure state and host-effect layers the
//! terminal driver uses.

use std::pin::pin;

use philo_agent_runtime::{
    AgentEvent, CancelReason, ModelCallId, OperationId, OperationStatus, SessionId,
    SettlementDurability, ToolBatchId, ToolCallId, ToolDisplay, ToolResult, TurnId, UserMessage,
};

use crate::api::confirmation::{ConfirmationRequest, ConfirmationResponse};
use crate::api::host::TuiHost;
use crate::app::action::Action;
use crate::app::effect::Effect;
use crate::app::state::App;
use crate::app::status::StatusData;
use crate::app::transcript::{InfoLevel, TranscriptLine};
use crate::driver::host_effects;
use crate::tests::support::{FakeHost, image_session_view, session_view};

fn app() -> App {
    App::new(StatusData::new("model-a", "s-1", InfoLevel::Default), true)
}

fn type_text(app: &mut App, text: &str) {
    for ch in text.chars() {
        app.on_action(Action::InsertChar(ch));
    }
}

fn collect(lines: Vec<TranscriptLine>, output: &mut Vec<String>) {
    output.extend(
        lines
            .into_iter()
            .map(|line| format!("{:?}: {}", line.kind, line.text)),
    );
}

async fn execute_effects(
    app: &mut App,
    host: &FakeHost,
    effects: Vec<Effect>,
    output: &mut Vec<String>,
) {
    for effect in effects {
        match effect {
            Effect::Append(lines) => collect(lines, output),
            Effect::Host(request) => {
                collect(host_effects::execute(app, host, request).await, output);
            }
            Effect::Submit { text, attachments } => {
                assert!(attachments.is_empty());
                let _ = host
                    .prompt(SessionId::new(&app.status.session), UserMessage::new(text))
                    .await;
            }
            other => output.push(format!("effect: {other:?}")),
        }
    }
}

#[tokio::test]
async fn fake_host_complete_interaction_snapshot() {
    let host = FakeHost::new();
    host.set_sessions(&["s-1", "s-image"]);
    host.set_view("s-1", session_view("s-1"));
    host.set_view("s-image", image_session_view("s-image"));
    let mut app = app();
    let mut output = Vec::new();

    // Session picker: lazy previews and an image-bearing history replay.
    type_text(&mut app, "/sessions");
    let effects = app.on_action(Action::Submit);
    execute_effects(&mut app, host.as_ref(), effects, &mut output).await;
    output.push(format!(
        "overlay:\n{}",
        app.overlay_frame(5).expect("picker").to_text()
    ));
    let effects = app.on_action(Action::MoveDown);
    execute_effects(&mut app, host.as_ref(), effects, &mut output).await;
    let effects = app.on_action(Action::Submit);
    execute_effects(&mut app, host.as_ref(), effects, &mut output).await;

    // Confirmation: request -> overlay -> y -> requester receives Allow.
    let channel = host.confirmations();
    let response = channel.request(ConfirmationRequest {
        title: "write workspace file".to_owned(),
        body: "path: src/main.rs".to_owned(),
    });
    let mut response = pin!(response);
    app.sync_confirmation(channel.front());
    output.push(format!(
        "overlay:\n{}",
        app.overlay_frame(5).expect("confirmation").to_text()
    ));
    let effects = app.on_action(Action::InsertChar('y'));
    execute_effects(&mut app, host.as_ref(), effects, &mut output).await;
    let answer = response.as_mut().await;
    output.push(format!("confirmation: {answer:?}"));
    assert_eq!(answer, ConfirmationResponse::Allow);

    // Two accepted prompts: the second takes the Busy/FIFO presentation path.
    type_text(&mut app, "first prompt");
    let effects = app.on_action(Action::Submit);
    execute_effects(&mut app, host.as_ref(), effects, &mut output).await;
    app.set_busy(true, 0);
    type_text(&mut app, "queued follow-up");
    let effects = app.on_action(Action::Submit);
    execute_effects(&mut app, host.as_ref(), effects, &mut output).await;
    assert_eq!(host.prompt_count(), 2);

    // Real-time M10/M11 vocabulary remains visible while no overlay swallows
    // it: seal notice, reasoning, tool dual channel, user cancellation.
    for event in [
        AgentEvent::OperationQueued {
            operation_id: OperationId::new("op-queued"),
        },
        AgentEvent::PriorTurnSealed {
            turn_id: TurnId::new("turn-old"),
        },
        AgentEvent::ReasoningDelta {
            model_call_id: ModelCallId::new("call-1"),
            text: "checking\n".to_owned(),
        },
        AgentEvent::TextDelta {
            delta: "answer line\n".to_owned(),
        },
        AgentEvent::ToolBatchRequested {
            tool_batch_id: ToolBatchId::new("batch-1"),
            call_count: 1,
        },
        AgentEvent::ToolExecutionStarted {
            tool_batch_id: ToolBatchId::new("batch-1"),
            tool_call_id: ToolCallId::new("tool-1"),
            index: 0,
            tool_name: "read".to_owned(),
            arguments: r#"{"path":"src/main.rs"}"#.to_owned(),
        },
        AgentEvent::ToolExecutionCompleted {
            tool_batch_id: ToolBatchId::new("batch-1"),
            tool_call_id: ToolCallId::new("tool-1"),
            index: 0,
            tool_name: "read".to_owned(),
            result: ToolResult::success("fn main() {}"),
            display: Some(ToolDisplay::new("read 12 bytes").with_fact("bytes", "12")),
        },
        AgentEvent::CancellationRequested {
            operation_id: OperationId::new("op-1"),
            reason: CancelReason::User,
        },
        AgentEvent::TurnCancelled {
            turn_id: TurnId::new("turn-1"),
            reason: CancelReason::User,
        },
        AgentEvent::OperationSettled {
            operation_id: OperationId::new("op-1"),
            status: OperationStatus::Cancelled,
            durability: SettlementDurability::Confirmed,
        },
    ] {
        let effects = app.on_agent_event(&event);
        execute_effects(&mut app, host.as_ref(), effects, &mut output).await;
    }

    crate::tests::assert_tui_snapshot!("m12_complete_interaction", output.join("\n"));
}
