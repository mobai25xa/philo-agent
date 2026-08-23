//! Complete interaction: session selection, history, confirmation, and
//! queueing through the same pure state the terminal driver uses.

use philo_agent_service::{
    ConfirmationDecision, FrontendOperationEvent, FrontendSessionSummary, FrontendToolDisplay,
    FrontendToolResult, FrontendUpdateKind,
};

use crate::app::action::Action;
use crate::app::effect::Effect;
use crate::app::state::App;
use crate::app::status::StatusData;
use crate::app::transcript::{InfoLevel, TranscriptLine};
use crate::tests::support::{frontend_update, image_session_view, session_view};

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
            .map(|line| format!("{:?}: {}", line.kind, line.text.trim_end_matches('\n'))),
    );
}

fn apply_kind(app: &mut App, kind: FrontendUpdateKind, output: &mut Vec<String>) {
    for effect in app.apply_update(&frontend_update(1, kind)) {
        match effect {
            Effect::Append(lines) => collect(lines, output),
            Effect::Host(_) => {}
            other => output.push(format!("effect: {other:?}")),
        }
    }
}

#[test]
fn frontend_complete_interaction_snapshot() {
    let mut app = app();
    let mut output = Vec::new();

    type_text(&mut app, "/sessions");
    for effect in app.on_action(Action::Submit) {
        match effect {
            Effect::Append(lines) => collect(lines, &mut output),
            Effect::Host(_) => apply_kind(
                &mut app,
                FrontendUpdateKind::SessionListLoaded {
                    sessions: vec![
                        FrontendSessionSummary {
                            session_id: "s-1".to_owned(),
                            title: Some("count the files".to_owned()),
                        },
                        FrontendSessionSummary {
                            session_id: "s-image".to_owned(),
                            title: None,
                        },
                    ],
                },
                &mut output,
            ),
            other => output.push(format!("effect: {other:?}")),
        }
    }
    if let Some(id) = app.claim_preview() {
        apply_kind(
            &mut app,
            FrontendUpdateKind::SessionPreviewed {
                session_id: id.clone(),
                view: session_view(&id),
            },
            &mut output,
        );
    }
    output.push(format!(
        "overlay:\n{}",
        app.overlay_frame(5).expect("picker").to_text()
    ));
    for effect in app.on_action(Action::MoveDown) {
        if let Effect::Host(crate::app::effect::HostRequest::LoadPreview(id)) = effect {
            apply_kind(
                &mut app,
                FrontendUpdateKind::SessionPreviewed {
                    session_id: id.clone(),
                    view: image_session_view(&id),
                },
                &mut output,
            );
        }
    }
    for effect in app.on_action(Action::Submit) {
        if let Effect::Host(crate::app::effect::HostRequest::SwitchSession(id)) = effect {
            apply_kind(
                &mut app,
                FrontendUpdateKind::SessionLoaded {
                    session_id: id.clone(),
                    view: image_session_view(&id),
                },
                &mut output,
            );
        }
    }

    apply_kind(
        &mut app,
        FrontendUpdateKind::ConfirmationRequested {
            confirmation_id: 1,
            title: "write workspace file".to_owned(),
            body: "path: src/main.rs".to_owned(),
        },
        &mut output,
    );
    output.push(format!(
        "overlay:\n{}",
        app.overlay_frame(5).expect("confirmation").to_text()
    ));
    let effects = app.on_action(Action::InsertChar('y'));
    let answered = effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::Host(crate::app::effect::HostRequest::Respond(
                1,
                ConfirmationDecision::Allow
            ))
        )
    });
    for effect in effects {
        if let Effect::Append(lines) = effect {
            collect(lines, &mut output);
        }
    }
    output.push(format!(
        "confirmation: {}",
        if answered { "Allow" } else { "missing" }
    ));

    type_text(&mut app, "first prompt");
    for effect in app.on_action(Action::Submit) {
        if let Effect::Append(lines) = effect {
            collect(lines, &mut output);
        }
    }
    if let Some(intent_id) = app.submit_state().intent_id() {
        for effect in app.on_action(Action::SubmitAccepted {
            intent_id,
            operation_id: "op-1".to_owned(),
        }) {
            if let Effect::Append(lines) = effect {
                collect(lines, &mut output);
            }
        }
    }
    app.set_busy(true, 0);
    type_text(&mut app, "queued follow-up");
    for effect in app.on_action(Action::Submit) {
        if let Effect::Append(lines) = effect {
            collect(lines, &mut output);
        }
    }
    if let Some(intent_id) = app.submit_state().intent_id() {
        for effect in app.on_action(Action::SubmitAccepted {
            intent_id,
            operation_id: "op-2".to_owned(),
        }) {
            if let Effect::Append(lines) = effect {
                collect(lines, &mut output);
            }
        }
    }

    for event in [
        FrontendOperationEvent::OperationQueued {
            operation_id: "op-queued".to_owned(),
        },
        FrontendOperationEvent::PriorTurnSealed {
            turn_id: "turn-old".to_owned(),
        },
        FrontendOperationEvent::ReasoningDelta {
            model_call_id: "call-1".to_owned(),
            text: "checking\n".to_owned(),
        },
        FrontendOperationEvent::TextDelta {
            delta: "answer line\n".to_owned(),
        },
        FrontendOperationEvent::ToolBatchRequested {
            tool_batch_id: "batch-1".to_owned(),
            call_count: 1,
        },
        FrontendOperationEvent::ToolExecutionStarted {
            tool_batch_id: "batch-1".to_owned(),
            tool_call_id: "tool-1".to_owned(),
            index: 0,
            tool_name: "read".to_owned(),
            arguments: r#"{"path":"src/main.rs"}"#.to_owned(),
        },
        FrontendOperationEvent::ToolExecutionCompleted {
            tool_batch_id: "batch-1".to_owned(),
            tool_call_id: "tool-1".to_owned(),
            index: 0,
            tool_name: "read".to_owned(),
            result: FrontendToolResult::Success {
                content: "fn main() {}".to_owned(),
            },
            display: Some(FrontendToolDisplay {
                detail: "read 12 bytes".to_owned(),
                facts: vec![("bytes".to_owned(), "12".to_owned())],
            }),
        },
        FrontendOperationEvent::CancellationRequested {
            operation_id: "op-1".to_owned(),
            reason: "User".to_owned(),
        },
        FrontendOperationEvent::TurnCancelled {
            turn_id: "turn-1".to_owned(),
            reason: "User".to_owned(),
        },
        FrontendOperationEvent::OperationSettled {
            operation_id: "op-1".to_owned(),
            session_id: "s-1".to_owned(),
            status: "Cancelled".to_owned(),
            durability: "Confirmed".to_owned(),
            session_revision: philo_agent_service::SettlementRevision::Unchanged,
        },
    ] {
        let before = app.cells.cells().len();
        let effects = app.on_operation_event(&event);
        for effect in effects {
            if let Effect::Append(lines) = effect {
                collect(lines, &mut output);
            }
        }
        collect(app.cells.cells()[before..].to_vec(), &mut output);
    }

    crate::tests::assert_tui_snapshot!("m12_complete_interaction", output.join("\n"));
}
