//! App state-machine tests.

use philo_agent_service::{
    ConfirmationDecision, FrontendOperationEvent, FrontendTokenUsage,
    FrontendUpdateKind, ServiceHealth,
};

use super::*;
use crate::app::action::Action;
use crate::app::command;
use crate::app::effect::{Effect, HostRequest};
use crate::app::overlay::PickerEntry;
use crate::app::status::StatusData;
use crate::app::transcript::{InfoLevel, LineKind, Tone, TranscriptLine};
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
    assert_eq!(texts(&appended(&committed)), ["", "hello", ""]);
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
fn disconnected_submit_restores_editable_draft_and_attachments() {
    use crate::app::submit::SubmitDispatchResult;

    let mut app = app();
    run(&mut app, "/image shots/a.png");
    let effects = run(&mut app, "keep me");
    let Effect::PrepareSubmit { intent_id, .. } = effects[0] else {
        panic!("expected PrepareSubmit");
    };

    app.on_action(Action::SubmitDispatchFinished {
        intent_id,
        result: SubmitDispatchResult::Disconnected {
            lane: "frontend-command",
        },
    });

    assert_eq!(app.input.text(), "keep me");
    assert_eq!(app.attachments().labels(), ["shots/a.png"]);
    assert!(matches!(
        app.submit_state(),
        crate::app::submit::SubmitState::Editing
    ));
}

#[test]
fn recovery_does_not_overwrite_a_newer_local_edit() {
    use crate::api::types::{TuiRecovery, TuiRecoveryAttachment};

    let mut app = app();
    type_text(&mut app, "newer");
    let restored = app.apply_recovery(TuiRecovery {
        draft: "older".to_owned(),
        attachments: vec![TuiRecoveryAttachment::Path("shots/a.png".to_owned())],
    });

    assert!(!restored);
    assert_eq!(app.input.text(), "newer");
    assert_eq!(app.attachments().labels(), ["shots/a.png"]);
}

#[test]
fn recovery_restores_once_into_a_pristine_composer() {
    use crate::api::types::{TuiRecovery, TuiRecoveryAttachment};

    let mut app = app();
    let recovery = TuiRecovery {
        draft: "preserved".to_owned(),
        attachments: vec![TuiRecoveryAttachment::Path("shots/a.png".to_owned())],
    };
    assert!(app.apply_recovery(recovery.clone()));
    assert_eq!(app.input.text(), "preserved");
    assert_eq!(app.attachments().labels(), ["shots/a.png"]);

    assert!(!app.apply_recovery(recovery));
    assert_eq!(app.input.text(), "preserved");
    assert_eq!(app.attachments().labels(), ["shots/a.png"]);
}

#[test]
fn loop_exit_captures_submit_still_waiting_for_media() {
    use crate::api::types::{TuiRecovery, TuiRecoveryAttachment};

    let mut app = app();
    run(&mut app, "/image shots/a.png");
    run(&mut app, "pending");

    assert_eq!(
        app.into_recovery(),
        Some(TuiRecovery {
            draft: "pending".to_owned(),
            attachments: vec![TuiRecoveryAttachment::Path("shots/a.png".to_owned())],
        })
    );
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
    app.set_busy(true);
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
    app.set_busy(true);
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
    app.set_busy(true);
    let effects = run(&mut app, "/new");
    assert!(host_requests(&effects).is_empty());
    assert_eq!(appended(&effects)[1].kind, LineKind::Error);
}

#[test]
fn models_opens_the_picker_even_while_busy() {
    let mut app = app();
    app.set_busy(true);
    assert_eq!(
        host_requests(&run(&mut app, "/models")),
        vec![HostRequest::OpenModels]
    );
}

#[test]
fn removed_model_and_reasoning_commands_are_unknown() {
    let mut app = app();
    // Bare `/model` never reaches the parser: it is a prefix of `/models`,
    // so the completion menu executes `/models`. Argument forms are truly
    // gone and report themselves.
    for command in ["/model other", "/reasoning high", "/reasoning"] {
        let effects = run(&mut app, command);
        assert!(host_requests(&effects).is_empty());
        assert!(
            appended(&effects)[1]
                .text
                .starts_with("unknown command: /"),
            "{command} must no longer be in the table"
        );
    }
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
            "what is this?",
            "[attached shots/a.png]",
            "[attached clipboard image (image/png, 2.0 KB)]",
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
    app.set_busy(true);
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
    app.set_busy(true);
    run(&mut app, "/quit");
    app.on_action(Action::InsertChar('x'));
    app.on_action(Action::Backspace);
    let effects = run(&mut app, "/quit");
    assert!(!effects.contains(&Effect::Quit), "it asks again");
}

/// The open menu's rows as plain `{usage}{summary}` text.
fn menu_rows(app: &App) -> Vec<String> {
    app.command_menu_frame(80, 10)
        .expect("menu is open")
        .rows
        .iter()
        .map(|row| format!("{}{}", row.usage, row.summary))
        .collect()
}

#[test]
fn typing_a_slash_opens_the_menu_and_typing_filters_it() {
    let mut app = app();
    type_text(&mut app, "/");
    let frame = app
        .command_menu_frame(80, command::COMMANDS.len())
        .expect("menu is open");
    assert_eq!(frame.rows.len(), command::COMMANDS.len());
    assert_eq!(frame.selected, 0);
    // The renderer's own cap keeps the list bounded.
    assert_eq!(
        app.command_menu_frame(80, 10)
            .expect("menu is open")
            .rows
            .len(),
        10
    );

    type_text(&mut app, "s");
    let frame = app.command_menu_frame(80, 10).expect("menu is open");
    assert_eq!(frame.rows.len(), 2);
    assert!(frame.rows[0].usage.starts_with("▶ /sessions"));
    assert_eq!(frame.rows[0].summary, "pick a session to continue");
    assert!(frame.rows[1].usage.starts_with("  /status"));

    type_text(&mut app, "e");
    let frame = app.command_menu_frame(80, 10).expect("menu is open");
    assert_eq!(frame.rows.len(), 1);
    assert_eq!(frame.selected, 0);
}

#[test]
fn enter_executes_the_highlighted_command() {
    let mut app = app();
    type_text(&mut app, "/se");
    let effects = app.on_action(Action::Submit);
    assert_eq!(host_requests(&effects), vec![HostRequest::OpenSessions]);
    assert_eq!(texts(&appended(&effects))[0], "/sessions");
    assert!(app.input.is_empty());
    assert!(app.command_menu_frame(80, 10).is_none());
}

#[test]
fn tab_accepts_the_highlighted_command() {
    let mut app = app();
    type_text(&mut app, "/s");
    app.on_action(Action::MoveDown);
    app.on_action(Action::Complete);
    assert_eq!(app.input.text(), "/status ");
    assert!(app.command_menu_frame(80, 10).is_none());

    // With the menu closed, Tab reopens it without changing the draft.
    app.on_action(Action::Backspace);
    app.on_action(Action::Escape);
    assert!(app.command_menu_frame(80, 10).is_none());
    app.on_action(Action::Complete);
    assert_eq!(app.input.text(), "/status");
    assert!(app.command_menu_frame(80, 10).is_some());
}

#[test]
fn menu_navigation_moves_the_highlight_only() {
    let mut app = app();
    run(&mut app, "older draft");
    accept_pending(&mut app);
    type_text(&mut app, "/s");
    let before = app.input.text();

    app.on_action(Action::MoveDown);
    assert_eq!(
        app.command_menu_frame(80, 10).expect("menu").selected,
        1,
        "down moves the highlight"
    );
    app.on_action(Action::MoveUp);
    app.on_action(Action::MoveUp);
    assert_eq!(
        app.command_menu_frame(80, 10).expect("menu").selected,
        0,
        "the first row does not move"
    );
    assert_eq!(app.input.text(), before, "the draft is untouched");
}

#[test]
fn escape_closes_the_completion_menu_without_cancelling() {
    let mut app = app();
    app.set_busy(true);
    type_text(&mut app, "/s");
    assert!(app.on_action(Action::Escape).is_empty(), "no cancel");
    assert!(app.command_menu_frame(80, 10).is_none());
    assert_eq!(app.on_action(Action::Escape), vec![Effect::CancelActive]);
}

#[test]
fn edits_reopen_and_close_the_menu() {
    let mut plain = app();
    let mut app = app();
    type_text(&mut app, "/s");
    assert_eq!(menu_rows(&app).len(), 2);

    // A word that matches nothing closes the menu; backspacing reopens it.
    type_text(&mut app, "z");
    assert!(app.command_menu_frame(80, 10).is_none());
    app.on_action(Action::Backspace);
    assert_eq!(menu_rows(&app).len(), 2);

    // An argument ends the command word and closes the menu.
    type_text(&mut app, " 0.5");
    assert!(app.command_menu_frame(80, 10).is_none());

    // Plain text never opens a menu.
    type_text(&mut plain, "plain text");
    assert!(plain.command_menu_frame(80, 10).is_none());
    plain.on_action(Action::Complete);
    assert!(plain.command_menu_frame(80, 10).is_none());
}

#[test]
fn noop_key_release_events_keep_the_menu_open() {
    let mut app = app();
    type_text(&mut app, "/s");
    assert_eq!(menu_rows(&app).len(), 2);
    // Kitty-protocol key releases surface as Action::None; they must be
    // inert instead of closing the menu between press and release.
    app.on_action(Action::None);
    assert_eq!(menu_rows(&app).len(), 2);
}

#[test]
fn non_editing_interactions_keep_the_menu_open() {
    let mut app = app();
    type_text(&mut app, "/s");
    for action in [
        Action::PageTranscriptUp,
        Action::PageTranscriptDown,
        Action::ScrollTranscript(3),
        Action::ToggleLevel,
        Action::Redraw,
        Action::MoveLeft,
        Action::MoveRight,
    ] {
        app.on_action(action.clone());
        assert_eq!(
            menu_rows(&app).len(),
            2,
            "{action:?} must not close the menu"
        );
    }
}

#[test]
fn ctrl_c_clearing_the_draft_closes_the_menu() {
    let mut app = app();
    type_text(&mut app, "/s");
    assert!(app.command_menu_frame(80, 10).is_some());
    app.on_action(Action::CtrlC);
    assert!(app.input.is_empty());
    assert!(app.command_menu_frame(80, 10).is_none());
}

#[test]
fn an_empty_slash_opens_the_whole_table() {
    let mut app = app();
    type_text(&mut app, "/");
    let frame = app
        .command_menu_frame(80, command::COMMANDS.len())
        .expect("menu is open");
    let rows = frame
        .rows
        .iter()
        .map(|row| format!("{}{}", row.usage, row.summary))
        .collect::<Vec<_>>();
    crate::tests::assert_tui_snapshot!("command_menu", rows.join("\n"));
}

#[test]
fn the_picker_moves_the_selection_and_loads_previews_lazily() {
    let mut app = app();
    app.open_picker(vec![
        PickerEntry::untitled("s-1"),
        PickerEntry::untitled("s-2"),
    ]);
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
    app.open_picker(vec![
        PickerEntry::untitled("s-1"),
        PickerEntry::untitled("s-2"),
    ]);
    app.on_action(Action::MoveDown);
    assert_eq!(
        host_requests(&app.on_action(Action::Submit)),
        vec![HostRequest::SwitchSession("s-2".to_owned())]
    );
    assert!(app.picker().is_none(), "Enter closes the overlay");

    app.open_picker(vec![PickerEntry::untitled("s-1")]);
    assert!(app.on_action(Action::Escape).is_empty());
    assert!(app.picker().is_none());
}

#[test]
fn the_picker_refuses_to_switch_while_a_turn_runs() {
    let mut app = app();
    app.set_busy(true);
    app.open_picker(vec![PickerEntry::untitled("s-1")]);
    let effects = app.on_action(Action::Submit);
    assert!(host_requests(&effects).is_empty());
    assert_eq!(appended(&effects)[0].kind, LineKind::Error);
    assert!(app.picker().is_some(), "the overlay stays open");
}

#[test]
fn the_picker_does_not_type_into_the_input() {
    let mut app = app();
    app.open_picker(vec![PickerEntry::untitled("s-1")]);
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
    app.open_picker(vec![PickerEntry::untitled("s-1")]);
    let (title, body) = request("run_command");
    app.sync_confirmation(Some((7, title, body)));
    let frame = app.overlay_frame(4).expect("an overlay is painted");
    assert!(frame.title.starts_with("Approval required"));
    // Answering restores the picker underneath.
    app.on_action(Action::InsertChar('n'));
    let frame = app.overlay_frame(4).expect("the picker is still open");
    assert!(frame.title.starts_with("Sessions"));
}

#[test]
fn overlays_never_swallow_agent_events() {
    let mut app = app();
    app.open_picker(vec![PickerEntry::untitled("s-1")]);
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
    app.set_busy(true);
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
    app.set_busy(true);
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
fn usage_events_update_the_usage_corner() {
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
                tone: Tone::Plain,
                header: None,
                body: None,
            },
            TranscriptLine {
                kind: LineKind::User,
                text: "hello".to_owned(),
                tone: Tone::Plain,
                header: None,
                body: None,
            },
            TranscriptLine {
                kind: LineKind::User,
                text: String::new(),
                tone: Tone::Plain,
                header: None,
                body: None,
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
                kind: LineKind::Meta,
                text: "queued behind the active turn".to_owned(),
                tone: Tone::Plain,
                header: None,
                body: None,
            },
            TranscriptLine {
                kind: LineKind::Meta,
                text: "previous turn did not end cleanly and was sealed; its tool \
                       calls may have executed without recorded results"
                    .to_owned(),
                tone: Tone::Plain,
                header: None,
                body: None,
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
    app.flush_stream();
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
        tone: Tone::Plain,
        header: None,
        body: None,
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
        tone: Tone::Plain,
        header: None,
        body: None,
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
    app.set_busy(true);

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
    app.set_busy(true);
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
    app.open_picker(vec![PickerEntry::untitled("s-1")]);
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

fn slice_texts(app: &App, width: usize, height: usize) -> Vec<String> {
    app.history_slice(width, height)
        .rows
        .iter()
        .map(|row| row.text.clone())
        .collect()
}

/// The v4.0 P3 re-skinned think header row.
fn is_think_header(text: &str) -> bool {
    text.starts_with("▎ Thought") && text.contains("按 Space 查看")
}

fn think_run() -> Vec<TranscriptLine> {
    vec![
        TranscriptLine {
            kind: LineKind::Meta,
            text: "before".to_owned(),
            tone: Tone::Plain,
            header: None,
            body: None,
        },
        TranscriptLine {
            kind: LineKind::Reasoning,
            text: "think".to_owned(),
            tone: Tone::Plain,
            header: None,
            body: None,
        },
        TranscriptLine {
            kind: LineKind::Reasoning,
            text: "  hidden thought".to_owned(),
            tone: Tone::Plain,
            header: None,
            body: None,
        },
        TranscriptLine {
            kind: LineKind::Meta,
            text: "after".to_owned(),
            tone: Tone::Plain,
            header: None,
            body: None,
        },
    ]
}

#[test]
fn sealed_think_blocks_fold_and_toggle_reopens() {
    let mut app = app();
    app.cells.push_closed(think_run());
    app.note_history_layout(80, 10);
    let texts = slice_texts(&app, 80, 10);
    assert_eq!(texts.len(), 3, "sealed think blocks fold their body: {texts:?}");
    assert_eq!(texts[0], "before");
    assert_eq!(texts[2], "after");
    assert!(
        is_think_header(&texts[1]),
        "the folded run renders the think header: {texts:?}"
    );

    assert!(app.toggle_reasoning_block(1, 0));
    let texts = slice_texts(&app, 80, 10);
    assert!(texts.iter().any(|text| text.contains("hidden thought")));
    assert!(
        is_think_header(texts.get(1).expect("header row")),
        "the header row keeps its re-skin: {texts:?}"
    );

    assert!(app.toggle_reasoning_block(1, 0));
    let texts = slice_texts(&app, 80, 10);
    assert_eq!(texts.len(), 3, "refolded to one header row: {texts:?}");
    assert_eq!(texts[0], "before");
    assert_eq!(texts[2], "after");
    assert!(is_think_header(&texts[1]));
}

#[test]
fn toggle_ignores_body_rows_and_non_reasoning_heads() {
    let mut app = app();
    app.cells.push_closed(think_run());
    app.note_history_layout(80, 10);
    assert!(!app.toggle_reasoning_block(2, 0), "run tail is not a head");
    assert!(!app.toggle_reasoning_block(0, 0), "meta cell is not think");
    assert!(!app.toggle_reasoning_block(3, 1), "only header rows toggle");
    assert_eq!(slice_texts(&app, 80, 10).len(), 3);
}

#[test]
fn streaming_think_starts_folded_and_expansion_survives_the_seal() {
    let mut fresh = app();
    let mut app = app();
    // Reducer shape: a sealed `think` header cell followed by the open body.
    app.cells.push_closed([TranscriptLine {
        kind: LineKind::Reasoning,
        text: "think".to_owned(),
        tone: Tone::Plain,
        header: None,
        body: None,
    }]);
    app.cells.begin(LineKind::Reasoning, "  partial thought");
    let texts = slice_texts(&app, 80, 10);
    assert_eq!(texts.len(), 1, "a streaming block stays folded: {texts:?}");
    assert!(
        is_think_header(&texts[0]),
        "the folded header wears the re-skin: {texts:?}"
    );

    app.toggle_reasoning_block(0, 0);
    let texts = slice_texts(&app, 80, 10);
    assert!(texts.iter().any(|text| text.contains("partial thought")));

    app.cells.close_open();
    let texts = slice_texts(&app, 80, 10);
    assert!(
        texts.iter().any(|text| text.contains("partial thought")),
        "manual expansion survives the seal"
    );

    fresh.cells.push_closed([TranscriptLine {
        kind: LineKind::Reasoning,
        text: "think".to_owned(),
        tone: Tone::Plain,
        header: None,
        body: None,
    }]);
    fresh.cells.begin(LineKind::Reasoning, "  partial thought");
    fresh.cells.close_open();
    let texts = slice_texts(&fresh, 80, 10);
    assert_eq!(texts.len(), 1, "sealing leaves an untouched stream folded: {texts:?}");
    assert!(is_think_header(&texts[0]));
}

#[test]
fn plain_click_on_a_think_header_toggles_it() {
    let mut app = app();
    app.cells.push_closed(vec![
        TranscriptLine {
            kind: LineKind::Meta,
            text: "row-0".to_owned(),
            tone: Tone::Plain,
            header: None,
            body: None,
        },
        TranscriptLine {
            kind: LineKind::Reasoning,
            text: "think".to_owned(),
            tone: Tone::Plain,
            header: None,
            body: None,
        },
        TranscriptLine {
            kind: LineKind::Reasoning,
            text: "  body".to_owned(),
            tone: Tone::Plain,
            header: None,
            body: None,
        },
    ]);
    app.note_history_layout(80, 4);
    app.on_action(Action::SelectStart { x: 5, y: 1 });
    app.on_action(Action::SelectEnd { x: 5, y: 1 });
    assert!(!app.has_selection(), "the click toggled instead");
    let texts = slice_texts(&app, 80, 4);
    assert!(texts.iter().any(|text| text.contains("│ body")));
}

#[test]
fn begin_session_forgets_manual_think_folds() {
    let mut app = app();
    app.cells.push_closed(think_run());
    app.note_history_layout(80, 10);
    app.toggle_reasoning_block(1, 0);
    assert!(slice_texts(&app, 80, 10).len() > 3);

    app.begin_session("other");
    app.cells.push_closed(think_run());
    assert_eq!(
        slice_texts(&app, 80, 10).len(),
        3,
        "the new session starts with fresh fold state"
    );
}

#[test]
fn text_delta_without_close_stays_open() {
    let mut app = app();
    let effects = app.on_operation_event(&FrontendOperationEvent::TextDelta {
        delta: "partial answer".to_owned(),
    });
    assert!(effects.is_empty(), "agent events write the store directly");
    app.flush_stream();
    assert_eq!(app.cells.open_index(), Some(0));
    assert!(app.cells.has_open());
    assert_eq!(
        app.cells.cells(),
        [TranscriptLine {
            kind: LineKind::Answer,
            text: "partial answer".to_owned(),
            tone: Tone::Plain,
            header: None,
            body: None,
        }]
    );
    let slice = app.history_slice(80, 3);
    assert_eq!(
        slice
            .rows
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>(),
        ["partial answer"]
    );
    assert_eq!(slice.rows[0].cell_index, 0);
}

#[test]
fn newline_stays_inside_one_open_answer_cell() {
    let mut app = app();
    app.on_operation_event(&FrontendOperationEvent::TextDelta {
        delta: "hello\nworld".to_owned(),
    });
    app.flush_stream();
    assert_eq!(app.cells.open_index(), Some(0));
    assert!(app.cells.has_open());
    assert_eq!(
        app.cells.cells(),
        [TranscriptLine {
            kind: LineKind::Answer,
            text: "hello\nworld".to_owned(),
            tone: Tone::Plain,
            header: None,
            body: None,
        }]
    );
    let texts: Vec<_> = app
        .history_slice(80, 5)
        .rows
        .iter()
        .map(|row| row.text.clone())
        .collect();
    assert_eq!(texts, ["hello", "world"]);
    assert_eq!(texts.iter().filter(|text| *text == "hello").count(), 1);
    assert_eq!(texts.iter().filter(|text| *text == "world").count(), 1);
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
    app.flush_stream();
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
    app.flush_stream();
    app.note_history_layout(80, 5);
    let slice = app.history_slice(80, 5);
    let last = slice.rows.last().expect("open cell visible");
    assert_eq!(last.text, "live tail");
    assert_eq!(last.cell_index, 3);

    app.on_action(Action::SelectStart { x: 0, y: 3 });
    app.on_action(Action::SelectDrag { x: 11, y: 3 });
    app.on_action(Action::SelectEnd { x: 11, y: 3 });
    assert_eq!(
        app.on_action(Action::CtrlC),
        vec![Effect::WriteClipboard("live tail".to_owned())]
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
    app.open_picker(vec![PickerEntry::untitled("s-1")]);
    app.on_action(Action::Home);
    assert!(app.follow_bottom());
    assert!(app.picker().is_some());
    app.on_action(Action::End);
    assert!(app.follow_bottom());
    assert!(app.picker().is_some());
}

#[test]
fn scrolling_stays_effective_during_busy_turns() {
    // Busy turns no longer lift or pin the viewport: the user's scroll is
    // simply never fought by an anchor state machine.
    let mut app = app();
    seed_rows(&mut app, 50);
    app.note_history_layout(80, 33);
    app.set_busy(true);
    app.on_action(Action::PageTranscriptUp);
    assert!(!app.follow_bottom(), "paging up unfollows");
    assert!(app.history_slice(80, 33).at_top);

    app.on_action(Action::End);
    app.on_action(Action::End);
    assert!(app.follow_bottom(), "double End returns to follow");
    assert!(app.history_slice(80, 33).at_bottom);
}

// -- P5 history browse mode ------------------------------------------------

#[test]
fn browse_mode_preserves_the_draft_attachments_and_caret() {
    let mut app = app();
    seed_rows(&mut app, 10);
    app.note_history_layout(80, 3);
    type_text(&mut app, "keep me");
    app.on_action(Action::MoveLeft);
    let cursor = app.input.cursor();
    app.attach_image("image/png".to_owned(), vec![0; 4], "clipboard image");

    app.on_action(Action::EnterBrowse);
    assert!(app.in_browse_mode(), "PgUp/Ctrl+U enters browse");
    assert!(!app.input_focused());
    assert_eq!(app.input.text(), "keep me", "the draft survives entry");
    assert_eq!(app.input.cursor(), cursor, "the caret survives entry");
    assert_eq!(app.attachments().len(), 1, "attachments survive entry");

    // The modal's own keys move the cursor; the composer stays untouched.
    app.on_action(Action::BrowseStep(1));
    app.on_action(Action::BrowseStep(-1));
    assert_eq!(app.input.text(), "keep me");
    assert_eq!(app.input.cursor(), cursor);

    app.on_action(Action::ExitBrowse);
    assert!(!app.in_browse_mode());
    assert!(app.input_focused());
    assert_eq!(app.input.text(), "keep me", "the draft survives the round trip");
    assert_eq!(app.input.cursor(), cursor, "the caret survives the round trip");
    assert_eq!(app.attachments().len(), 1);

    // Esc exits the same way, and the cursor clears outside browse mode.
    assert!(app.browse_cursor().is_none(), "no cursor outside browse");
    app.on_action(Action::EnterBrowse);
    assert!(app.browse_cursor().is_some());
    app.on_action(Action::Escape);
    assert!(!app.in_browse_mode());
    assert!(app.input_focused());
    assert_eq!(app.input.text(), "keep me");
}

#[test]
fn browse_entry_defers_to_the_confirmation_and_picker_layers() {
    let mut app = app();
    seed_rows(&mut app, 10);
    app.note_history_layout(80, 3);

    // P1: a pending confirmation owns the keyboard; PgUp is inert.
    let (title, body) = request("run_command");
    app.sync_confirmation(Some((1, title, body)));
    app.on_action(Action::EnterBrowse);
    assert!(app.has_confirmation(), "P1 confirmation stays up");
    assert!(!app.in_browse_mode(), "browse entry is blocked under P1");
    app.on_action(Action::InsertChar('y'));
    assert!(!app.has_confirmation());

    // P2: an open picker owns the keyboard; EnterBrowse cannot reach below.
    app.open_picker(vec![PickerEntry::untitled("s-1")]);
    app.on_action(Action::EnterBrowse);
    assert!(app.picker().is_some(), "P2 picker stays up");
    assert!(!app.in_browse_mode(), "browse entry is blocked under P2");
    app.on_action(Action::Escape);
    assert!(app.picker().is_none());
}

#[test]
fn entering_browse_from_a_slash_menu_closes_the_menu() {
    let mut app = app();
    seed_rows(&mut app, 10);
    app.note_history_layout(80, 3);
    type_text(&mut app, "/s");
    assert!(app.command_menu_frame(80, 10).is_some());

    app.on_action(Action::EnterBrowse);
    assert!(app.in_browse_mode(), "PgUp from a slash menu opens browse");
    assert!(
        app.command_menu_frame(80, 10).is_none(),
        "entering browse drops the menu"
    );
    assert_eq!(app.input.text(), "/s", "the slash draft survives");
}

#[test]
fn browse_steps_move_the_cursor_and_clamp_at_the_transcript_ends() {
    let mut app = app();
    seed_rows(&mut app, 10);
    app.note_history_layout(80, 3);
    app.on_action(Action::EnterBrowse);
    assert_eq!(
        app.browse_cursor(),
        Some((7, 0)),
        "entry takes over the current window's top row"
    );

    for _ in 0..5 {
        app.on_action(Action::BrowseStep(1));
    }
    assert_eq!(app.browse_cursor(), Some((9, 0)), "steps clamp at the tail");

    app.on_action(Action::BrowseStep(-2));
    assert_eq!(app.browse_cursor(), Some((7, 0)));
    for _ in 0..10 {
        app.on_action(Action::BrowseStep(-1));
    }
    assert_eq!(app.browse_cursor(), Some((0, 0)), "steps clamp at the head");

    app.on_action(Action::Home);
    assert_eq!(app.browse_cursor(), Some((0, 0)));
    app.on_action(Action::End);
    assert_eq!(app.browse_cursor(), Some((9, 0)), "End jumps to the last row");
}

#[test]
fn browse_pages_step_a_whole_window_and_stay_in_view() {
    let mut app = app();
    seed_rows(&mut app, 10);
    app.note_history_layout(80, 8);
    app.on_action(Action::EnterBrowse);
    assert_eq!(app.browse_cursor(), Some((2, 0)));

    // PgUp rides the viewport up by the full height; the cursor follows by
    // a page (height - 2) and clamps at the head.
    app.on_action(Action::BrowsePage(-1));
    assert_eq!(app.browse_cursor(), Some((0, 0)));
    assert!(app.history_slice(80, 8).at_top, "PgUp reaches the head");

    // PgDn rides back toward the tail, following once the window bottoms.
    app.on_action(Action::BrowsePage(1));
    assert!(app.history_slice(80, 8).at_bottom, "PgDn re-follows the tail");
    assert!(app.follow_bottom());
    assert_eq!(app.browse_cursor(), Some((6, 0)));

    // Re-entering after the re-follow resumes at the tail again.
    app.on_action(Action::ExitBrowse);
    app.on_action(Action::EnterBrowse);
    assert_eq!(app.browse_cursor(), Some((2, 0)));
}

#[test]
fn browse_toggle_fold_hits_think_heads_and_ignores_plain_rows() {
    // Think head: Space expands and refolds the run.
    let mut app = app();
    app.cells.push_closed(think_run());
    app.note_history_layout(80, 10);
    app.on_action(Action::EnterBrowse);
    app.on_action(Action::BrowseStep(1));
    assert_eq!(app.browse_cursor(), Some((1, 0)));
    assert_eq!(slice_texts(&app, 80, 10).len(), 3, "sealed runs start folded");
    app.on_action(Action::BrowseToggleFold);
    assert!(
        slice_texts(&app, 80, 10)
            .iter()
            .any(|text| text.contains("hidden thought")),
        "Space opens the think body"
    );
    app.on_action(Action::BrowseToggleFold);
    assert_eq!(slice_texts(&app, 80, 10).len(), 3, "Space refolds the run");

    // Plain row: no-op, the transcript stays untouched.
    app.on_action(Action::Home);
    assert_eq!(app.browse_cursor(), Some((0, 0)));
    app.on_action(Action::BrowseToggleFold);
    assert_eq!(slice_texts(&app, 80, 10).len(), 3);
}

#[test]
fn browse_toggle_fold_hits_tool_card_bodies() {
    use crate::app::transcript::{CardBody, SegSpan};
    let mut app = app();
    app.cells.push_closed(vec![
        crate::app::transcript::line(LineKind::Meta, "before"),
        crate::app::transcript::line(LineKind::Meta, "head"),
        TranscriptLine {
            kind: LineKind::Tool,
            text: String::new(),
            tone: Tone::Plain,
            header: None,
            body: Some(CardBody {
                lines: (0..4)
                    .map(|i| vec![SegSpan::plain(format!("line-{i}"))])
                    .collect(),
                threshold: 2,
                fold_default_collapsed: true,
                fold_count: 2,
                fold_label: "行已折叠".to_owned(),
                fold_hint: true,
                fold_all: false,
            }),
        },
        crate::app::transcript::line(LineKind::Meta, "after"),
    ]);
    app.note_history_layout(80, 10);
    app.on_action(Action::EnterBrowse);
    app.on_action(Action::BrowseStep(1));
    app.on_action(Action::BrowseStep(1));
    assert_eq!(app.browse_cursor(), Some((2, 0)));
    assert!(app.tool_card_collapsed_at(2), "bodies fold past the threshold");
    app.on_action(Action::BrowseToggleFold);
    assert!(!app.tool_card_collapsed_at(2), "Space opens the body");
    app.on_action(Action::BrowseToggleFold);
    assert!(app.tool_card_collapsed_at(2), "Space refolds the body");
}

#[test]
fn submit_from_browse_mode_exits_and_follows_the_turn() {
    let mut app = app();
    seed_rows(&mut app, 20);
    app.note_history_layout(80, 3);
    type_text(&mut app, "from browse");
    app.on_action(Action::EnterBrowse);
    app.on_action(Action::Home);
    assert!(!app.follow_bottom(), "browsing away from the tail pins the view");

    let effects = app.on_action(Action::Submit);
    assert!(!app.in_browse_mode(), "Enter from browse mode leaves the modal");
    assert!(app.follow_bottom(), "the turn follows the tail again");
    assert_eq!(app.input.text(), "", "the preserved draft was consumed");
    assert!(
        matches!(effects.as_slice(), [Effect::PrepareSubmit { intent_id: 1, .. }]),
        "the draft sends: {effects:?}"
    );

    // An empty draft: Enter still leaves browse and returns to the tail.
    let mut fresh = App::new(StatusData::new("m", "s", InfoLevel::Default), true);
    seed_rows(&mut fresh, 20);
    fresh.note_history_layout(80, 3);
    fresh.on_action(Action::EnterBrowse);
    fresh.on_action(Action::Home);
    assert!(!fresh.follow_bottom());
    assert!(fresh.on_action(Action::Submit).is_empty(), "nothing to send");
    assert!(!fresh.in_browse_mode());
    assert!(fresh.follow_bottom());
}

#[test]
fn ctrl_c_in_browse_mode_interrupts_and_exits_the_modal() {
    let mut app = app();
    seed_rows(&mut app, 10);
    app.note_history_layout(80, 3);
    app.set_busy(true);
    app.on_action(Action::EnterBrowse);
    let effects = app.on_action(Action::CtrlC);
    assert_eq!(effects, vec![Effect::InterruptCancel]);
    assert!(!app.in_browse_mode(), "Ctrl+C leaves browse mode");
    assert!(app.input_focused());
}

#[test]
fn mouse_scroll_passes_through_browse_mode() {
    let mut app = app();
    seed_rows(&mut app, 20);
    app.note_history_layout(80, 3);
    app.on_action(Action::EnterBrowse);
    app.on_action(Action::ScrollTranscript(-3));
    assert!(app.in_browse_mode(), "wheel stays inside the modal");
    assert!(!app.follow_bottom());
}

#[test]
fn browse_on_an_empty_transcript_is_safe() {
    let mut app = app();
    app.note_history_layout(80, 3);
    app.on_action(Action::EnterBrowse);
    assert!(app.in_browse_mode());
    assert_eq!(app.browse_cursor(), Some((0, 0)));
    app.on_action(Action::BrowseStep(-5));
    app.on_action(Action::BrowsePage(1));
    app.on_action(Action::BrowseToggleFold);
    app.on_action(Action::End);
    assert_eq!(app.browse_cursor(), Some((0, 0)));
    assert!(app.on_action(Action::ExitBrowse).is_empty());
    assert!(app.input_focused());
}
