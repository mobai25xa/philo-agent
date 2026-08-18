//! App state-machine tests.

use philo_agent_service::{
    ConfirmationDecision, FrontendOperationEvent, FrontendReasoningEffort, FrontendTokenUsage,
    FrontendUpdateKind, ServiceHealth,
};

use super::*;
use crate::app::action::Action;
use crate::app::command;
use crate::app::effect::{Effect, HostRequest};
use crate::app::status::StatusData;
use crate::app::transcript::{InfoLevel, LineKind, TranscriptLine};
use crate::tests::support::{frontend_update, idle_snapshot, session_view};

fn app() -> App {
    App::new(StatusData::new("m", "s", InfoLevel::Default), true)
}

fn type_text(app: &mut App, text: &str) {
    for ch in text.chars() {
        app.on_action(Action::InsertChar(ch));
    }
}

/// Submits `text` and returns the produced effects.
fn run(app: &mut App, text: &str) -> Vec<Effect> {
    type_text(app, text);
    app.on_action(Action::Submit)
}

/// Completes a pending submit so another draft can be sent.
fn accept_pending(app: &mut App) -> Vec<Effect> {
    let intent_id = app.submit_state().intent_id().expect("pending intent");
    app.on_action(Action::SubmitAccepted {
        intent_id,
        operation_id: format!("op-{intent_id}"),
    })
}

/// The transcript lines of the first Append effect.
fn appended(effects: &[Effect]) -> Vec<TranscriptLine> {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Append(lines) => Some(lines.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn texts(lines: &[TranscriptLine]) -> Vec<String> {
    lines.iter().map(|line| line.text.clone()).collect()
}

fn host_requests(effects: &[Effect]) -> Vec<HostRequest> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Host(request) => Some(request.clone()),
            _ => None,
        })
        .collect()
}

fn request(title: &str) -> (String, String) {
    (title.to_owned(), format!("{title} body"))
}

#[test]
fn submit_holds_pending_intent_without_transcript_commit() {
    let mut app = app();
    let effects = run(&mut app, "hello");
    assert_eq!(
        effects,
        vec![Effect::PrepareSubmit {
            intent_id: 1,
            text: "hello".to_owned(),
            attachments: Vec::new(),
        },]
    );
    assert!(app.input.is_empty());
    assert!(matches!(
        app.submit_state(),
        crate::app::submit::SubmitState::Dispatching(_)
    ));

    let committed = app.on_action(Action::SubmitAccepted {
        intent_id: 1,
        operation_id: "op-1".to_owned(),
    });
    assert_eq!(texts(&appended(&committed)), ["", "› hello", ""]);
}

#[test]
fn backpressured_submit_restores_draft_and_attachments() {
    use crate::app::submit::SubmitDispatchResult;

    let mut app = app();
    run(&mut app, "/image shots/a.png");
    type_text(&mut app, "keep me");
    let effects = app.on_action(Action::Submit);
    let Effect::PrepareSubmit {
        intent_id,
        attachments,
        ..
    } = &effects[0]
    else {
        panic!("expected PrepareSubmit");
    };
    assert_eq!(attachments.len(), 1);
    let intent_id = *intent_id;

    let restored = app.on_action(Action::SubmitDispatchFinished {
        intent_id,
        result: SubmitDispatchResult::Backpressured,
    });
    assert!(
        texts(&appended(&restored))
            .iter()
            .any(|line| line.contains("服务繁忙，提交未发送"))
    );
    assert_eq!(app.input.text(), "keep me");
    assert_eq!(app.attachments().len(), 1);
    assert_eq!(app.attachments().labels(), vec!["shots/a.png".to_owned()]);
    assert!(matches!(
        app.submit_state(),
        crate::app::submit::SubmitState::Editing
    ));
}

#[test]
fn backpressured_submit_does_not_push_history() {
    use crate::app::submit::SubmitDispatchResult;

    let mut app = app();
    run(&mut app, "failed");
    let intent_id = app.submit_state().intent_id().unwrap();
    let _ = app.on_action(Action::SubmitDispatchFinished {
        intent_id,
        result: SubmitDispatchResult::Backpressured,
    });
    assert_eq!(app.input.text(), "failed");
    for _ in "failed".chars() {
        app.on_action(Action::Backspace);
    }
    run(&mut app, "ok");
    accept_pending(&mut app);

    type_text(&mut app, "x");
    app.on_action(Action::MoveUp);
    assert_eq!(app.input.text(), "ok");
    app.on_action(Action::MoveUp);
    assert_eq!(
        app.input.text(),
        "ok",
        "backpressured draft must not enter history"
    );
}

#[test]
fn slash_command_still_enters_history() {
    let mut app = app();
    run(&mut app, "/help");
    type_text(&mut app, "x");
    app.on_action(Action::MoveUp);
    assert_eq!(app.input.text(), "/help");
}

#[test]
fn rejected_submit_restores_without_committing_transcript() {
    use crate::app::submit::SubmitDispatchResult;
    use philo_agent_service::CommandReject;

    let mut app = app();
    run(&mut app, "draft");
    let intent_id = app.submit_state().intent_id().unwrap();
    let _ = app.on_action(Action::SubmitDispatchFinished {
        intent_id,
        result: SubmitDispatchResult::Enqueued(philo_agent_service::FrontendRequestId::new(42)),
    });
    let effects = app.on_action(Action::SubmitCommandRejected {
        intent_id,
        reason: CommandReject::NoCurrentSession,
    });
    assert!(
        texts(&appended(&effects))
            .iter()
            .any(|line| line.contains("no current session"))
    );
    assert_eq!(app.input.text(), "draft");
    assert!(
        app.cells
            .cells()
            .iter()
            .all(|line| line.kind != LineKind::User),
        "rejected submit must not commit a user transcript block"
    );
}

#[test]
fn late_submit_accepted_after_new_draft_is_ignored() {
    let mut app = app();
    run(&mut app, "old");
    let old_intent = app.submit_state().intent_id().unwrap();
    let _ = app.on_action(Action::SubmitDispatchFinished {
        intent_id: old_intent,
        result: crate::app::submit::SubmitDispatchResult::Backpressured,
    });
    run(&mut app, "new");
    let new_intent = app.submit_state().intent_id().unwrap();
    assert_ne!(old_intent, new_intent);

    let late = app.on_action(Action::SubmitAccepted {
        intent_id: old_intent,
        operation_id: "op-stale".to_owned(),
    });
    assert!(late.is_empty());
    assert_eq!(app.submit_state().intent_id(), Some(new_intent));
    assert!(app.input.is_empty());
}

#[test]
fn empty_submit_is_a_no_op() {
    let mut app = app();
    assert!(app.on_action(Action::Submit).is_empty());
}

#[test]
fn attachments_only_submit_prepares() {
    let mut app = app();
    run(&mut app, "/image shots/a.png");
    let effects = app.on_action(Action::Submit);
    assert_eq!(
        effects,
        vec![Effect::PrepareSubmit {
            intent_id: 1,
            text: String::new(),
            attachments: vec![crate::app::attachment::PendingAttachment::Path(
                "shots/a.png".to_owned()
            )],
        }]
    );
    assert!(app.input.is_empty());
    assert!(app.attachments().is_empty());
    assert!(matches!(
        app.submit_state(),
        crate::app::submit::SubmitState::Dispatching(_)
    ));
}

#[test]
fn typing_during_dispatch_backpressure_keeps_attachments() {
    use crate::app::submit::SubmitDispatchResult;

    let mut app = app();
    run(&mut app, "/image shots/a.png");
    let effects = run(&mut app, "keep-text");
    let Effect::PrepareSubmit { intent_id, .. } = effects[0] else {
        panic!("expected PrepareSubmit");
    };
    type_text(&mut app, "newer");
    let restored = app.on_action(Action::SubmitDispatchFinished {
        intent_id,
        result: SubmitDispatchResult::Backpressured,
    });
    assert!(
        texts(&appended(&restored))
            .iter()
            .any(|line| line.contains("服务繁忙，提交未发送"))
    );
    assert_eq!(app.input.text(), "newer");
    assert_eq!(app.attachments().len(), 1);
    assert_eq!(app.attachments().labels(), vec!["shots/a.png".to_owned()]);
    assert!(matches!(
        app.submit_state(),
        crate::app::submit::SubmitState::Editing
    ));
}

#[test]
fn snapshot_while_dispatching_restores_draft_and_attachments() {
    let mut app = app();
    run(&mut app, "/image shots/a.png");
    run(&mut app, "draft");
    let effects = app.apply_update(&frontend_update(
        1,
        FrontendUpdateKind::SnapshotReady(Box::new(idle_snapshot("s"))),
    ));
    assert!(
        texts(&appended(&effects))
            .iter()
            .any(|line| line.contains("提交未确认"))
    );
    assert_eq!(app.input.text(), "draft");
    assert_eq!(app.attachments().labels(), vec!["shots/a.png".to_owned()]);
    assert!(matches!(
        app.submit_state(),
        crate::app::submit::SubmitState::Editing
    ));
}

#[test]
fn snapshot_accepts_when_live_contains_landed_draft() {
    use philo_agent_service::FrontendAvailability;

    let mut app = app();
    run(&mut app, "count the files");
    let mut snapshot = idle_snapshot("s");
    snapshot.durable_session_view = Some(session_view("s"));
    snapshot.availability = FrontendAvailability::Busy {
        operation_id: "op-landed".to_owned(),
    };
    snapshot.live.operation_id = Some("op-landed".to_owned());
    let effects = app.apply_update(&frontend_update(
        1,
        FrontendUpdateKind::SnapshotReady(Box::new(snapshot)),
    ));
    assert!(
        texts(&appended(&effects))
            .iter()
            .any(|line| line.contains("count the files"))
    );
    assert!(matches!(
        app.submit_state(),
        crate::app::submit::SubmitState::Accepted {
            operation_id,
            ..
        } if operation_id == "op-landed"
    ));
}

#[test]
fn epoch_ended_while_dispatching_restores_draft_and_attachments() {
    let mut app = app();
    run(&mut app, "/image shots/a.png");
    run(&mut app, "draft");
    app.set_busy(true, 0);
    let effects = app.apply_update(&frontend_update(
        1,
        FrontendUpdateKind::ServiceHealthChanged {
            health: ServiceHealth::RuntimeEpochEnded {
                message: "runtime epoch ended".to_owned(),
            },
        },
    ));
    assert!(
        texts(&appended(&effects))
            .iter()
            .any(|line| line.contains("runtime epoch ended"))
    );
    assert_eq!(app.input.text(), "draft");
    assert_eq!(app.attachments().labels(), vec!["shots/a.png".to_owned()]);
    assert!(!app.status.busy);
    assert!(matches!(
        app.submit_state(),
        crate::app::submit::SubmitState::Editing
    ));
}

#[test]
fn busy_submit_notes_the_queueing() {
    let mut app = app();
    app.set_busy(true, 0);
    let effects = run(&mut app, "next");
    let lines = appended(&effects);
    assert!(lines.iter().any(|line| line.text.contains("queued")));
    assert_eq!(
        effects[1],
        Effect::PrepareSubmit {
            intent_id: 1,
            text: "next".to_owned(),
            attachments: Vec::new(),
        }
    );
}

#[test]
fn unknown_commands_never_submit_to_the_model() {
    let mut app = app();
    let effects = run(&mut app, "/definitely-unknown");
    assert_eq!(effects.len(), 1, "echo and error only");
    let lines = appended(&effects);
    assert_eq!(lines[0].text, "/definitely-unknown", "the command echoes");
    assert_eq!(lines[1].kind, LineKind::Error);
    assert_eq!(
        lines[1].text,
        "unknown command: /definitely-unknown (try /help)"
    );
}

#[test]
fn help_lists_the_whole_command_table() {
    let mut app = app();
    let lines = texts(&appended(&run(&mut app, "/help")));
    for spec in command::COMMANDS {
        assert!(
            lines.iter().any(|line| line.contains(spec.usage)),
            "missing {} in {lines:?}",
            spec.usage
        );
    }
}

#[test]
fn help_snapshot() {
    let mut app = app();
    crate::tests::assert_tui_snapshot!(
        "app_help",
        texts(&appended(&run(&mut app, "/help"))).join("\n")
    );
}

#[test]
fn new_and_sessions_go_through_the_host() {
    let mut app = app();
    assert_eq!(
        host_requests(&run(&mut app, "/new")),
        vec![HostRequest::NewSession]
    );
    assert_eq!(
        host_requests(&run(&mut app, "/sessions")),
        vec![HostRequest::OpenSessions]
    );
}

#[test]
fn a_new_session_is_refused_while_a_turn_runs() {
    let mut app = app();
    app.set_busy(true, 0);
    let effects = run(&mut app, "/new");
    assert!(host_requests(&effects).is_empty());
    assert_eq!(appended(&effects)[1].kind, LineKind::Error);
}

#[test]
fn model_needs_a_name_and_works_while_busy() {
    let mut app = app();
    let lines = appended(&run(&mut app, "/model"));
    assert_eq!(lines[1].text, "usage: /model <name>");

    app.set_busy(true, 0);
    assert_eq!(
        host_requests(&run(&mut app, "/model other")),
        vec![HostRequest::RebuildModel("other".to_owned())]
    );
}

#[test]
fn reasoning_maps_levels_and_reports_bad_ones() {
    let mut app = app();
    assert_eq!(
        host_requests(&run(&mut app, "/reasoning high")),
        vec![HostRequest::SetReasoning(FrontendReasoningEffort::High)]
    );
    let effects = run(&mut app, "/reasoning turbo");
    assert!(host_requests(&effects).is_empty());
    assert!(
        appended(&effects)[1]
            .text
            .starts_with("unknown reasoning level: turbo")
    );
    assert!(
        appended(&run(&mut app, "/reasoning"))[1]
            .text
            .starts_with("usage: /reasoning")
    );
}

#[test]
fn image_registers_a_path_for_the_next_message() {
    let mut app = app();
    assert!(
        appended(&run(&mut app, "/image"))[1]
            .text
            .starts_with("usage: /image")
    );
    let lines = appended(&run(&mut app, "/image shots/a.png"));
    assert_eq!(
        lines[1].text,
        "image queued: shots/a.png (1 waiting for the next message)"
    );
    run(&mut app, "/image shots/b.png");
    assert_eq!(app.attachments().labels(), ["shots/a.png", "shots/b.png"]);
}

#[test]
fn a_clipboard_image_joins_the_queue_and_rides_the_next_message() {
    let mut app = app();
    run(&mut app, "/image shots/a.png");
    let effects = app.attach_image("image/png".to_owned(), vec![0; 2048], "clipboard image");
    assert_eq!(
        texts(&appended(&effects)),
        ["attached: clipboard image (image/png, 2.0 KB) (2 waiting for the next message)"]
    );

    let effects = run(&mut app, "what is this?");
    let Effect::PrepareSubmit { attachments, .. } = &effects[0] else {
        panic!("the message carries its attachments");
    };
    assert_eq!(attachments.len(), 2);
    assert!(app.attachments().is_empty(), "the queue drains on send");

    let committed = app.on_action(Action::SubmitAccepted {
        intent_id: 1,
        operation_id: "op-1".to_owned(),
    });
    assert_eq!(
        texts(&appended(&committed)),
        [
            "",
            "› what is this?",
            "  [attached shots/a.png]",
            "  [attached clipboard image (image/png, 2.0 KB)]",
            "",
        ]
    );
}

#[test]
fn a_refused_message_returns_to_the_input_with_its_survivors() {
    let mut app = app();
    run(&mut app, "/image missing.png");
    let effects = run(&mut app, "look");
    let Effect::PrepareSubmit {
        intent_id,
        text,
        attachments,
    } = effects[0].clone()
    else {
        panic!("submit carries the draft");
    };
    assert_eq!(text, "look");
    // The driver could not read one of them and hands back the rest.
    let _ = app.on_action(Action::SubmitMediaRefused {
        intent_id,
        kept: attachments[1..].to_vec(),
        errors: vec!["missing.png: not found".to_owned()],
    });
    assert_eq!(app.input.text(), "look");
}

#[test]
fn ctrl_v_asks_the_driver_for_the_clipboard() {
    let mut app = app();
    assert_eq!(app.on_action(Action::Paste), vec![Effect::ReadClipboard]);
    let effects = app.clipboard_unavailable("clipboard is empty");
    assert_eq!(
        texts(&appended(&effects)),
        ["no image on the clipboard (clipboard is empty); attach a file with /image <path>"]
    );
    assert!(app.attachments().is_empty());
}

#[test]
fn verbose_command_matches_the_toggle_chord() {
    let mut app = app();
    let lines = appended(&run(&mut app, "/verbose"));
    assert_eq!(lines[1].text, "info level: verbose");
    assert_eq!(app.level(), InfoLevel::Verbose);
    app.on_action(Action::ToggleLevel);
    assert_eq!(app.level(), InfoLevel::Default);
}

#[test]
fn status_and_config_go_through_the_host() {
    let mut app = app();
    assert_eq!(
        host_requests(&run(&mut app, "/status")),
        vec![HostRequest::ShowStatus]
    );
    assert_eq!(
        host_requests(&run(&mut app, "/config")),
        vec![HostRequest::ShowConfig]
    );
}

#[test]
fn quit_asks_once_while_a_turn_runs() {
    let mut app = app();
    app.set_busy(true, 0);
    let effects = run(&mut app, "/quit");
    assert!(!effects.contains(&Effect::Quit), "the first ask warns");
    assert!(appended(&effects)[1].text.contains("/quit again"));
    assert!(run(&mut app, "/quit").contains(&Effect::RequestShutdown));
}

#[test]
fn quit_leaves_immediately_when_idle() {
    let mut app = app();
    assert!(run(&mut app, "/quit").contains(&Effect::Quit));
}

#[test]
fn anything_between_two_quits_disarms_the_running_turn_exit() {
    let mut app = app();
    app.set_busy(true, 0);
    run(&mut app, "/quit");
    app.on_action(Action::InsertChar('x'));
    app.on_action(Action::Backspace);
    let effects = run(&mut app, "/quit");
    assert!(!effects.contains(&Effect::Quit), "it asks again");
}

#[test]
fn tab_completes_a_unique_command_and_cycles_ambiguous_ones() {
    let mut app = app();
    type_text(&mut app, "/se");
    app.on_action(Action::Complete);
    assert_eq!(app.input.text(), "/sessions ");
    assert!(app.completion_line().is_none());

    app.input.clear();
    type_text(&mut app, "/s");
    app.on_action(Action::Complete);
    assert_eq!(app.input.text(), "/s", "the shared prefix is already typed");
    assert_eq!(
        app.completion_line(),
        Some("commands: sessions status".to_owned())
    );
    app.on_action(Action::Complete);
    assert_eq!(app.input.text(), "/sessions");
    assert_eq!(
        app.completion_line(),
        Some("commands: [sessions] status".to_owned())
    );
    app.on_action(Action::Complete);
    assert_eq!(app.input.text(), "/status");
    app.on_action(Action::Complete);
    assert_eq!(app.input.text(), "/sessions", "the cycle wraps");
}

#[test]
fn tab_on_an_empty_slash_opens_the_whole_table() {
    let mut app = app();
    type_text(&mut app, "/");
    app.on_action(Action::Complete);
    crate::tests::assert_tui_snapshot!(
        "command_completion",
        app.completion_line().expect("menu is open")
    );
}

#[test]
fn escape_closes_the_completion_menu_without_cancelling() {
    let mut app = app();
    app.set_busy(true, 0);
    type_text(&mut app, "/s");
    app.on_action(Action::Complete);
    assert!(app.on_action(Action::Escape).is_empty(), "no cancel");
    assert!(app.completion_line().is_none());
    assert_eq!(app.on_action(Action::Escape), vec![Effect::CancelActive]);
}

#[test]
fn typing_closes_the_completion_menu() {
    let mut app = app();
    type_text(&mut app, "/s");
    app.on_action(Action::Complete);
    app.on_action(Action::InsertChar('t'));
    assert!(app.completion_line().is_none());
}

#[test]
fn tab_without_a_slash_does_nothing() {
    let mut app = app();
    type_text(&mut app, "plain");
    app.on_action(Action::Complete);
    assert_eq!(app.input.text(), "plain");
    assert!(app.completion_line().is_none());
}

#[test]
fn the_picker_moves_the_selection_and_loads_previews_lazily() {
    let mut app = app();
    app.open_picker(vec!["s-1".to_owned(), "s-2".to_owned()]);
    assert_eq!(app.claim_preview(), Some("s-1".to_owned()));

    let effects = app.on_action(Action::MoveDown);
    assert_eq!(
        host_requests(&effects),
        vec![HostRequest::LoadPreview("s-2".to_owned())]
    );
    assert!(
        app.on_action(Action::MoveDown).is_empty(),
        "the last entry does not move"
    );
    let effects = app.on_action(Action::MoveUp);
    assert!(
        host_requests(&effects).is_empty(),
        "s-1 was already claimed"
    );
}

#[test]
fn the_picker_switches_on_enter_and_closes_on_escape() {
    let mut app = app();
    app.open_picker(vec!["s-1".to_owned(), "s-2".to_owned()]);
    app.on_action(Action::MoveDown);
    assert_eq!(
        host_requests(&app.on_action(Action::Submit)),
        vec![HostRequest::SwitchSession("s-2".to_owned())]
    );
    assert!(app.picker().is_none(), "Enter closes the overlay");

    app.open_picker(vec!["s-1".to_owned()]);
    assert!(app.on_action(Action::Escape).is_empty());
    assert!(app.picker().is_none());
}

#[test]
fn the_picker_refuses_to_switch_while_a_turn_runs() {
    let mut app = app();
    app.set_busy(true, 0);
    app.open_picker(vec!["s-1".to_owned()]);
    let effects = app.on_action(Action::Submit);
    assert!(host_requests(&effects).is_empty());
    assert_eq!(appended(&effects)[0].kind, LineKind::Error);
    assert!(app.picker().is_some(), "the overlay stays open");
}

#[test]
fn the_picker_does_not_type_into_the_input() {
    let mut app = app();
    app.open_picker(vec!["s-1".to_owned()]);
    app.on_action(Action::InsertChar('x'));
    app.on_paste("pasted");
    assert!(app.input.is_empty());
}

#[test]
fn approval_answers_are_binary_and_echoed() {
    let mut app = app();
    let (title, body) = request("run_command");
    app.sync_confirmation(Some((1, title, body)));
    let effects = app.on_action(Action::InsertChar('y'));
    assert_eq!(appended(&effects)[0].text, "allowed: run_command");
    assert_eq!(
        host_requests(&effects),
        vec![HostRequest::Respond(1, ConfirmationDecision::Allow)]
    );
    assert!(app.confirm_prompt().is_none());

    for (index, action) in [Action::InsertChar('n'), Action::Escape, Action::CtrlC]
        .into_iter()
        .enumerate()
    {
        let id = index as u64 + 2;
        let (title, body) = request("write_file");
        app.sync_confirmation(Some((id, title, body)));
        let effects = app.on_action(action);
        assert_eq!(appended(&effects)[0].text, "denied: write_file");
        assert_eq!(
            host_requests(&effects),
            vec![HostRequest::Respond(id, ConfirmationDecision::Deny)]
        );
    }
}

#[test]
fn an_auto_denied_request_closes_the_overlay() {
    let mut app = app();
    let (title, body) = request("run_command");
    app.sync_confirmation(Some((1, title, body)));
    assert!(app.overlay_frame(4).is_some());
    // The channel denied everything when the operation settled.
    app.sync_confirmation(None);
    assert!(app.confirm_prompt().is_none());
    assert!(app.overlay_frame(4).is_none());
}

#[test]
fn the_approval_overlay_wins_over_the_picker() {
    let mut app = app();
    app.open_picker(vec!["s-1".to_owned()]);
    let (title, body) = request("run_command");
    app.sync_confirmation(Some((7, title, body)));
    let frame = app.overlay_frame(4).expect("an overlay is painted");
    assert!(frame.title.starts_with("approval required"));
    // Answering restores the picker underneath.
    app.on_action(Action::InsertChar('n'));
    let frame = app.overlay_frame(4).expect("the picker is still open");
    assert!(frame.title.starts_with("sessions"));
}

#[test]
fn overlays_never_swallow_agent_events() {
    let mut app = app();
    app.open_picker(vec!["s-1".to_owned()]);
    let (title, body) = request("run_command");
    app.sync_confirmation(Some((1, title, body)));
    let effects = app.on_operation_event(&FrontendOperationEvent::OperationSettled {
        operation_id: "op-1".to_owned(),
        session_id: "s-1".to_owned(),
        status: "Succeeded".to_owned(),
        durability: "Confirmed".to_owned(),
        session_revision: philo_agent_service::SettlementRevision::Unchanged,
    });
    assert!(effects.is_empty(), "agent events write the store directly");
    assert!(
        app.cells.is_empty(),
        "successful settlement stays off the transcript"
    );
}

#[test]
fn ctrl_c_clears_nonempty_input_first() {
    let mut app = app();
    type_text(&mut app, "draft");
    assert!(app.on_action(Action::CtrlC).is_empty());
    assert!(app.input.is_empty());
}

#[test]
fn ctrl_c_cancels_while_busy() {
    let mut app = app();
    app.set_busy(true, 0);
    assert_eq!(app.on_action(Action::CtrlC), vec![Effect::InterruptCancel]);
}

#[test]
fn ctrl_c_twice_quits_when_idle_and_empty() {
    let mut app = app();
    let first = app.on_action(Action::CtrlC);
    assert!(appended(&first)[0].text.contains("again to exit"));
    assert_eq!(app.on_action(Action::CtrlC), vec![Effect::Quit]);
}

#[test]
fn any_other_key_disarms_the_exit() {
    let mut app = app();
    app.on_action(Action::CtrlC);
    app.on_action(Action::InsertChar('x'));
    app.on_action(Action::Backspace);
    let effects = app.on_action(Action::CtrlC);
    let Effect::Append(_) = &effects[0] else {
        panic!("re-armed, not quit");
    };
}

#[test]
fn escape_cancels_only_while_busy() {
    let mut app = app();
    assert!(app.on_action(Action::Escape).is_empty());
    app.set_busy(true, 0);
    assert_eq!(app.on_action(Action::Escape), vec![Effect::CancelActive]);
}

#[test]
fn ctrl_d_quits_only_on_empty_input() {
    let mut app = app();
    type_text(&mut app, "x");
    assert!(app.on_action(Action::CtrlD).is_empty());
    app.on_action(Action::Backspace);
    assert_eq!(app.on_action(Action::CtrlD), vec![Effect::Quit]);
}

#[test]
fn input_history_recalls_previous_submissions() {
    let mut app = app();
    run(&mut app, "first");
    accept_pending(&mut app);
    run(&mut app, "second");
    accept_pending(&mut app);

    type_text(&mut app, "dra");
    app.on_action(Action::MoveUp);
    assert_eq!(app.input.text(), "second");
    app.on_action(Action::MoveUp);
    assert_eq!(app.input.text(), "first");
    app.on_action(Action::MoveDown);
    assert_eq!(app.input.text(), "second");
    app.on_action(Action::MoveDown);
    assert_eq!(app.input.text(), "dra", "the stash comes back");
}

#[test]
fn multiline_draft_moves_within_lines_before_history() {
    let mut app = app();
    run(&mut app, "top");
    accept_pending(&mut app);
    type_text(&mut app, "a");
    app.on_action(Action::InsertNewline);
    type_text(&mut app, "b");
    // Cursor on line 2: the first MoveUp moves within the draft.
    app.on_action(Action::MoveUp);
    assert_eq!(app.input.text(), "a\nb");
    // On line 1 now: the next MoveUp recalls history.
    app.on_action(Action::MoveUp);
    assert_eq!(app.input.text(), "top");
}

#[test]
fn toggle_level_flips_and_reports() {
    let mut app = app();
    assert_eq!(app.level(), InfoLevel::Default);
    app.on_action(Action::ToggleLevel);
    assert_eq!(app.level(), InfoLevel::Verbose);
    assert_eq!(app.status.level, InfoLevel::Verbose);
}

#[test]
fn usage_events_update_the_status_bar() {
    let mut app = app();
    let usage = FrontendTokenUsage {
        input_tokens: Some(5),
        output_tokens: Some(7),
        ..FrontendTokenUsage::default()
    };
    app.on_operation_event(&FrontendOperationEvent::ModelUsageUpdated {
        model_call_id: "m-1".to_owned(),
        usage,
    });
    assert_eq!(app.status.usage, Some(usage));
}

#[test]
fn config_reload_applies_show_reasoning_and_reports_success() {
    use philo_agent_service::FrontendConfigEntry;
    let mut app = app();
    let effects = app.apply_update(&frontend_update(
        1,
        FrontendUpdateKind::ConfigChanged {
            entries: vec![
                FrontendConfigEntry {
                    key: "ui.show_reasoning".to_owned(),
                    value: "false".to_owned(),
                    source: "project".to_owned(),
                },
                FrontendConfigEntry {
                    key: "context_window".to_owned(),
                    value: "8000".to_owned(),
                    source: "project".to_owned(),
                },
                FrontendConfigEntry {
                    key: "model".to_owned(),
                    value: "model-b".to_owned(),
                    source: "project".to_owned(),
                },
            ],
        },
    ));
    assert!(!app.shows_reasoning());
    assert_eq!(app.status.model, "model-b");
    assert_eq!(app.status.context_window, Some(8_000));
    assert_eq!(texts(&appended(&effects)), ["config reloaded"]);
}

#[test]
fn config_listing_is_distinct_from_hot_reload() {
    use philo_agent_service::FrontendConfigEntry;
    let mut app = app();
    app.expect_config_listing = true;
    let effects = app.apply_update(&frontend_update(
        1,
        FrontendUpdateKind::ConfigChanged {
            entries: vec![FrontendConfigEntry {
                key: "model".to_owned(),
                value: "model-b".to_owned(),
                source: "project".to_owned(),
            }],
        },
    ));
    assert!(texts(&appended(&effects))[0].contains("config (effective)"));
}

#[test]
fn generation_install_failure_is_an_error_line() {
    let mut app = app();
    app.pending_model_switch = true;
    let effects = app.apply_update(&frontend_update(
        1,
        FrontendUpdateKind::GenerationInstallFailed {
            name: "broken".to_owned(),
            message: "config not reloaded: invalid TOML".to_owned(),
        },
    ));
    let lines = appended(&effects);
    assert_eq!(lines[0].kind, LineKind::Error);
    assert!(lines[0].text.contains("invalid TOML"));
}

fn concatenated_appends(effects: &[Effect]) -> Vec<TranscriptLine> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Append(lines) => Some(lines.as_slice()),
            _ => None,
        })
        .flatten()
        .cloned()
        .collect()
}

#[test]
fn submit_ingests_append_payloads_into_cells() {
    let mut app = app();
    run(&mut app, "hello");
    let effects = accept_pending(&mut app);
    let expected = concatenated_appends(&effects);
    assert_eq!(
        expected,
        vec![
            TranscriptLine {
                kind: LineKind::User,
                text: String::new(),
            },
            TranscriptLine {
                kind: LineKind::User,
                text: "› hello".to_owned(),
            },
            TranscriptLine {
                kind: LineKind::User,
                text: String::new(),
            },
        ]
    );
    assert_eq!(app.cells.cells(), expected.as_slice());
}

#[test]
fn agent_events_apply_into_cells() {
    let mut app = app();
    let queued = app.on_operation_event(&FrontendOperationEvent::OperationQueued {
        operation_id: "op-1".to_owned(),
    });
    let sealed = app.on_operation_event(&FrontendOperationEvent::PriorTurnSealed {
        turn_id: "old-turn".to_owned(),
    });
    assert!(queued.is_empty(), "agent events write the store directly");
    assert!(sealed.is_empty(), "agent events write the store directly");
    assert_eq!(
        app.cells.cells(),
        [
            TranscriptLine {
                kind: LineKind::Notice,
                text: "queued behind the active turn".to_owned(),
            },
            TranscriptLine {
                kind: LineKind::Notice,
                text: "previous turn did not end cleanly and was sealed; its tool \
                       calls may have executed without recorded results"
                    .to_owned(),
            },
        ]
    );
}

#[test]
fn begin_session_clears_ingested_cells() {
    let mut app = app();
    run(&mut app, "hello");
    app.on_operation_event(&FrontendOperationEvent::TextDelta {
        delta: "partial".to_owned(),
    });
    assert!(!app.cells.is_empty());
    assert!(app.cells.has_open());
    app.begin_session("other");
    assert!(app.cells.is_empty());
    assert!(app.cells.cells().is_empty());
    assert!(!app.cells.has_open());
    assert!(app.follow_bottom());
}

#[test]
fn page_up_unfollows_after_layout_is_noted() {
    let mut app = app();
    app.cells.push_closed((0..20).map(|i| TranscriptLine {
        kind: LineKind::Meta,
        text: format!("row-{i}"),
    }));
    app.note_history_layout(80, 3);
    assert!(app.follow_bottom());
    app.on_action(Action::PageTranscriptUp);
    assert!(!app.follow_bottom());
    app.on_action(Action::PageTranscriptDown);
    assert!(app.follow_bottom());
    app.on_action(Action::ScrollTranscript(-3));
    assert!(!app.follow_bottom());
}

#[test]
fn redraw_returns_hard_redraw_and_does_not_clear_cells() {
    let mut app = app();
    run(&mut app, "hello");
    accept_pending(&mut app);
    let before = app.cells.cells().to_vec();
    assert!(!before.is_empty());
    let effects = app.on_action(Action::Redraw);
    assert_eq!(effects, vec![Effect::HardRedraw]);
    assert_eq!(app.cells.cells(), before.as_slice());
}

fn seed_rows(app: &mut App, count: usize) {
    app.cells.push_closed((0..count).map(|i| TranscriptLine {
        kind: LineKind::Meta,
        text: format!("row-{i}"),
    }));
}

#[test]
fn mouse_select_unfollows_and_copies_visual_text() {
    let mut app = app();
    seed_rows(&mut app, 10);
    app.note_history_layout(80, 3);
    assert!(app.follow_bottom());

    app.on_action(Action::SelectStart { x: 0, y: 0 });
    assert!(!app.follow_bottom());
    app.on_action(Action::SelectDrag { x: 5, y: 0 });
    app.on_action(Action::SelectEnd { x: 5, y: 0 });
    assert!(app.has_selection());

    let effects = app.on_action(Action::CtrlC);
    assert_eq!(effects, vec![Effect::WriteClipboard("row-7".to_owned())]);
    assert!(app.has_selection(), "copy keeps the highlight");
}

#[test]
fn collapsed_click_does_not_copy() {
    let mut app = app();
    seed_rows(&mut app, 10);
    app.note_history_layout(80, 3);
    app.on_action(Action::SelectStart { x: 1, y: 0 });
    app.on_action(Action::SelectEnd { x: 1, y: 0 });
    assert!(!app.has_selection());
    let first = app.on_action(Action::CtrlC);
    assert!(appended(&first)[0].text.contains("again to exit"));
}

#[test]
fn click_outside_history_clears_selection() {
    let mut app = app();
    seed_rows(&mut app, 10);
    app.note_history_layout(80, 3);
    app.on_action(Action::SelectStart { x: 0, y: 0 });
    app.on_action(Action::SelectDrag { x: 3, y: 0 });
    app.on_action(Action::SelectEnd { x: 3, y: 0 });
    assert!(app.has_selection());
    app.on_action(Action::SelectStart { x: 0, y: 20 });
    assert!(!app.has_selection());
}

#[test]
fn ctrl_c_copies_before_clearing_input_or_cancelling() {
    let mut app = app();
    seed_rows(&mut app, 10);
    app.note_history_layout(80, 3);
    type_text(&mut app, "draft");
    app.on_action(Action::SelectStart { x: 0, y: 0 });
    app.on_action(Action::SelectDrag { x: 3, y: 0 });
    app.on_action(Action::SelectEnd { x: 3, y: 0 });
    app.set_busy(true, 0);

    assert_eq!(
        app.on_action(Action::CtrlC),
        vec![Effect::WriteClipboard("row".to_owned())]
    );
    assert_eq!(app.input.text(), "draft");
}

#[test]
fn typing_clears_selection_and_escape_clears_then_cancels() {
    let mut app = app();
    seed_rows(&mut app, 10);
    app.note_history_layout(80, 3);
    app.on_action(Action::SelectStart { x: 0, y: 0 });
    app.on_action(Action::SelectDrag { x: 3, y: 0 });
    app.on_action(Action::SelectEnd { x: 3, y: 0 });
    app.on_action(Action::InsertChar('x'));
    assert!(!app.has_selection());

    app.on_action(Action::SelectStart { x: 0, y: 0 });
    app.on_action(Action::SelectDrag { x: 3, y: 0 });
    app.on_action(Action::SelectEnd { x: 3, y: 0 });
    assert!(app.on_action(Action::Escape).is_empty());
    assert!(!app.has_selection());

    app.on_action(Action::SelectStart { x: 0, y: 0 });
    app.on_action(Action::SelectDrag { x: 3, y: 0 });
    app.on_action(Action::SelectEnd { x: 3, y: 0 });
    app.set_busy(true, 0);
    assert_eq!(app.on_action(Action::Escape), vec![Effect::CancelActive]);
    assert!(!app.has_selection());
}

#[test]
fn scroll_keeps_cell_space_selection() {
    let mut app = app();
    seed_rows(&mut app, 10);
    app.note_history_layout(80, 3);
    app.on_action(Action::SelectStart { x: 0, y: 0 });
    app.on_action(Action::SelectDrag { x: 5, y: 0 });
    app.on_action(Action::SelectEnd { x: 5, y: 0 });
    app.on_action(Action::ScrollTranscript(-3));
    assert_eq!(
        app.on_action(Action::CtrlC),
        vec![Effect::WriteClipboard("row-7".to_owned())]
    );
}

#[test]
fn overlay_does_not_start_transcript_selection() {
    let mut app = app();
    seed_rows(&mut app, 10);
    app.note_history_layout(80, 3);
    app.open_picker(vec!["s-1".to_owned()]);
    app.on_action(Action::SelectStart { x: 0, y: 0 });
    app.on_action(Action::SelectDrag { x: 5, y: 0 });
    app.on_action(Action::SelectEnd { x: 5, y: 0 });
    assert!(!app.has_selection());
}

#[test]
fn begin_session_clears_selection() {
    let mut app = app();
    seed_rows(&mut app, 10);
    app.note_history_layout(80, 3);
    app.on_action(Action::SelectStart { x: 0, y: 0 });
    app.on_action(Action::SelectDrag { x: 5, y: 0 });
    app.on_action(Action::SelectEnd { x: 5, y: 0 });
    app.begin_session("other");
    assert!(!app.has_selection());
    assert!(app.follow_bottom());
}

#[test]
fn text_delta_without_close_stays_open() {
    let mut app = app();
    let effects = app.on_operation_event(&FrontendOperationEvent::TextDelta {
        delta: "partial answer".to_owned(),
    });
    assert!(effects.is_empty(), "agent events write the store directly");
    assert_eq!(app.cells.open_index(), Some(0));
    assert!(app.cells.has_open());
    assert_eq!(
        app.cells.cells(),
        [TranscriptLine {
            kind: LineKind::Answer,
            text: "partial answer".to_owned(),
        }]
    );
    let slice = app.history_slice(80, 3);
    assert_eq!(
        slice
            .rows
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>(),
        ["• partial answer"]
    );
    assert_eq!(slice.rows[0].cell_index, 0);
}

#[test]
fn newline_stays_inside_one_open_answer_cell() {
    let mut app = app();
    app.on_operation_event(&FrontendOperationEvent::TextDelta {
        delta: "hello\nworld".to_owned(),
    });
    assert_eq!(app.cells.open_index(), Some(0));
    assert!(app.cells.has_open());
    assert_eq!(
        app.cells.cells(),
        [TranscriptLine {
            kind: LineKind::Answer,
            text: "hello\nworld".to_owned(),
        }]
    );
    let texts: Vec<_> = app
        .history_slice(80, 5)
        .rows
        .iter()
        .map(|row| row.text.clone())
        .collect();
    assert_eq!(texts, ["• hello", "  world"]);
    assert_eq!(texts.iter().filter(|text| *text == "• hello").count(), 1);
    assert_eq!(texts.iter().filter(|text| *text == "  world").count(), 1);
}

#[test]
fn show_reasoning_off_produces_no_open_think_cells() {
    let mut app = App::new(StatusData::new("m", "s", InfoLevel::Default), false);
    app.on_operation_event(&FrontendOperationEvent::ReasoningDelta {
        model_call_id: "call-1".to_owned(),
        text: "secret thoughts".to_owned(),
    });
    assert!(app.cells.cells().is_empty());
    assert!(!app.cells.has_open());
    assert!(app.history_slice(80, 3).rows.is_empty());
}

#[test]
fn open_rows_do_not_yank_a_pinned_view() {
    let mut app = app();
    seed_rows(&mut app, 20);
    app.note_history_layout(80, 3);
    app.on_action(Action::PageTranscriptUp);
    let before: Vec<_> = app
        .history_slice(80, 3)
        .rows
        .iter()
        .map(|row| row.text.clone())
        .collect();
    assert!(!app.follow_bottom());

    app.on_operation_event(&FrontendOperationEvent::TextDelta {
        delta: "streaming tail".to_owned(),
    });
    let after: Vec<_> = app
        .history_slice(80, 3)
        .rows
        .iter()
        .map(|row| row.text.clone())
        .collect();
    assert_eq!(before, after);
    assert!(!app.follow_bottom());
    let open = app.cells.open_index().expect("stream still open");
    assert_eq!(app.cells.cells()[open].text, "streaming tail");
}

#[test]
fn copy_includes_open_text_via_display_cell_indices() {
    let mut app = app();
    seed_rows(&mut app, 3);
    app.on_operation_event(&FrontendOperationEvent::TextDelta {
        delta: "live tail".to_owned(),
    });
    app.note_history_layout(80, 5);
    let slice = app.history_slice(80, 5);
    let last = slice.rows.last().expect("open cell visible");
    assert_eq!(last.text, "• live tail");
    assert_eq!(last.cell_index, 3);

    app.on_action(Action::SelectStart { x: 0, y: 3 });
    app.on_action(Action::SelectDrag { x: 11, y: 3 });
    app.on_action(Action::SelectEnd { x: 11, y: 3 });
    assert_eq!(
        app.on_action(Action::CtrlC),
        vec![Effect::WriteClipboard("• live tail".to_owned())]
    );
}

#[test]
fn home_and_end_prefer_the_editing_line_then_jump_transcript() {
    let mut app = app();
    seed_rows(&mut app, 10);
    app.note_history_layout(80, 3);
    type_text(&mut app, "ab");
    app.on_action(Action::MoveLeft);
    assert_eq!(app.input.cursor(), (0, 1));

    app.on_action(Action::Home);
    assert_eq!(app.input.cursor(), (0, 0));
    assert!(app.follow_bottom(), "first Home stays in the composer");

    app.on_action(Action::Home);
    assert!(!app.follow_bottom());
    let top = app.history_slice(80, 3);
    assert!(top.at_top);
    assert_eq!(top.rows[0].text, "row-0");

    app.on_action(Action::End);
    assert_eq!(app.input.cursor(), (0, 2));
    assert!(!app.follow_bottom(), "first End stays in the composer");

    app.on_action(Action::End);
    assert!(app.follow_bottom());
    let bottom = app.history_slice(80, 3);
    assert!(bottom.at_bottom);
    assert_eq!(
        bottom.rows.last().map(|row| row.text.as_str()),
        Some("row-9")
    );
}

#[test]
fn home_on_an_empty_prompt_jumps_to_transcript_top() {
    let mut app = app();
    seed_rows(&mut app, 10);
    app.note_history_layout(80, 3);
    assert!(app.input.at_line_start());
    assert!(app.input.at_line_end());
    app.on_action(Action::Home);
    assert!(!app.follow_bottom());
    assert!(app.history_slice(80, 3).at_top);
    app.on_action(Action::End);
    assert!(app.follow_bottom());
    assert!(app.history_slice(80, 3).at_bottom);
}

#[test]
fn overlay_ignores_home_and_end() {
    let mut app = app();
    seed_rows(&mut app, 10);
    app.note_history_layout(80, 3);
    app.open_picker(vec!["s-1".to_owned()]);
    app.on_action(Action::Home);
    assert!(app.follow_bottom());
    assert!(app.picker().is_some());
    app.on_action(Action::End);
    assert!(app.follow_bottom());
    assert!(app.picker().is_some());
}
