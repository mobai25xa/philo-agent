//! App state-machine tests.

use philo_agent_runtime::AgentEvent;
use philo_session::SessionId;

use super::*;
use crate::api::confirmation::{ConfirmationId, ConfirmationRequest, ConfirmationResponse};
use crate::app::action::Action;
use crate::app::command;
use crate::app::effect::{Effect, HostRequest};
use crate::app::status::StatusData;
use crate::app::transcript::{InfoLevel, LineKind, TranscriptLine};

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

fn request(title: &str) -> ConfirmationRequest {
    ConfirmationRequest {
        title: title.to_owned(),
        body: format!("{title} body"),
    }
}

#[test]
fn submit_echoes_the_message_and_requests_a_prompt() {
    let mut app = app();
    let effects = run(&mut app, "hello");
    assert_eq!(
        effects,
        vec![
            Effect::Append(vec![
                TranscriptLine {
                    kind: LineKind::User,
                    text: "You".to_owned(),
                },
                TranscriptLine {
                    kind: LineKind::User,
                    text: "  hello".to_owned(),
                },
            ]),
            Effect::Submit {
                text: "hello".to_owned(),
                attachments: Vec::new(),
            },
        ]
    );
    assert!(app.input.is_empty());
}

#[test]
fn empty_submit_is_a_no_op() {
    let mut app = app();
    assert!(app.on_action(Action::Submit).is_empty());
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
        Effect::Submit {
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
fn model_needs_a_name_and_an_idle_runtime() {
    let mut app = app();
    let lines = appended(&run(&mut app, "/model"));
    assert_eq!(lines[1].text, "usage: /model <name>");

    app.set_busy(true, 0);
    let effects = run(&mut app, "/model other");
    assert!(host_requests(&effects).is_empty(), "busy refuses");
    assert!(appended(&effects)[1].text.contains("still on m"));

    app.set_busy(false, 0);
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
        vec![HostRequest::SetReasoning(
            philo_agent_runtime::ReasoningEffort::High
        )]
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
    assert_eq!(
        texts(&appended(&effects)),
        [
            "You",
            "  what is this?",
            "  [attached shots/a.png]",
            "  [attached clipboard image (image/png, 2.0 KB)]",
        ]
    );
    let Effect::Submit { attachments, .. } = &effects[1] else {
        panic!("the message carries its attachments");
    };
    assert_eq!(attachments.len(), 2);
    assert!(app.attachments().is_empty(), "the queue drains on send");
}

#[test]
fn a_refused_message_returns_to_the_input_with_its_survivors() {
    let mut app = app();
    run(&mut app, "/image missing.png");
    let effects = run(&mut app, "look");
    let Effect::Submit { text, attachments } = effects[1].clone() else {
        panic!("submit carries the draft");
    };
    // The driver could not read one of them and hands back the rest.
    app.restore_draft(&text, attachments[1..].to_vec());
    assert_eq!(app.input.text(), "look");
    assert!(app.attachments().is_empty());
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
    assert!(run(&mut app, "/quit").contains(&Effect::Quit));
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
    app.open_picker(vec![SessionId::new("s-1"), SessionId::new("s-2")]);
    assert_eq!(app.claim_preview(), Some(SessionId::new("s-1")));

    let effects = app.on_action(Action::MoveDown);
    assert_eq!(
        host_requests(&effects),
        vec![HostRequest::LoadPreview(SessionId::new("s-2"))]
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
    app.open_picker(vec![SessionId::new("s-1"), SessionId::new("s-2")]);
    app.on_action(Action::MoveDown);
    assert_eq!(
        host_requests(&app.on_action(Action::Submit)),
        vec![HostRequest::SwitchSession(SessionId::new("s-2"))]
    );
    assert!(app.picker().is_none(), "Enter closes the overlay");

    app.open_picker(vec![SessionId::new("s-1")]);
    assert!(app.on_action(Action::Escape).is_empty());
    assert!(app.picker().is_none());
}

#[test]
fn the_picker_refuses_to_switch_while_a_turn_runs() {
    let mut app = app();
    app.set_busy(true, 0);
    app.open_picker(vec![SessionId::new("s-1")]);
    let effects = app.on_action(Action::Submit);
    assert!(host_requests(&effects).is_empty());
    assert_eq!(appended(&effects)[0].kind, LineKind::Error);
    assert!(app.picker().is_some(), "the overlay stays open");
}

#[test]
fn the_picker_does_not_type_into_the_input() {
    let mut app = app();
    app.open_picker(vec![SessionId::new("s-1")]);
    app.on_action(Action::InsertChar('x'));
    app.on_paste("pasted");
    assert!(app.input.is_empty());
}

#[test]
fn approval_answers_are_binary_and_echoed() {
    let mut app = app();
    app.sync_confirmation(Some((ConfirmationId::for_test(1), request("run_command"))));
    let effects = app.on_action(Action::InsertChar('y'));
    assert_eq!(appended(&effects)[0].text, "allowed: run_command");
    assert_eq!(
        host_requests(&effects),
        vec![HostRequest::Respond(
            ConfirmationId::for_test(1),
            ConfirmationResponse::Allow
        )]
    );
    assert!(app.confirm_prompt().is_none());

    for (index, action) in [Action::InsertChar('n'), Action::Escape, Action::CtrlC]
        .into_iter()
        .enumerate()
    {
        let id = ConfirmationId::for_test(index as u64 + 2);
        app.sync_confirmation(Some((id, request("write_file"))));
        let effects = app.on_action(action);
        assert_eq!(appended(&effects)[0].text, "denied: write_file");
        assert_eq!(
            host_requests(&effects),
            vec![HostRequest::Respond(id, ConfirmationResponse::Deny)]
        );
    }
}

#[test]
fn an_auto_denied_request_closes_the_overlay() {
    let mut app = app();
    app.sync_confirmation(Some((ConfirmationId::for_test(1), request("run_command"))));
    assert!(app.overlay_frame(4).is_some());
    // The channel denied everything when the operation settled.
    app.sync_confirmation(None);
    assert!(app.confirm_prompt().is_none());
    assert!(app.overlay_frame(4).is_none());
}

#[test]
fn the_approval_overlay_wins_over_the_picker() {
    let mut app = app();
    app.open_picker(vec![SessionId::new("s-1")]);
    app.sync_confirmation(Some((ConfirmationId::for_test(7), request("run_command"))));
    let frame = app.overlay_frame(4).expect("an overlay is painted");
    assert!(frame.title.starts_with("approval required"));
    // Answering restores the picker underneath.
    app.on_action(Action::InsertChar('n'));
    let frame = app.overlay_frame(4).expect("the picker is still open");
    assert!(frame.title.starts_with("sessions"));
}

#[test]
fn overlays_never_swallow_agent_events() {
    use philo_agent_runtime::{OperationId, OperationStatus, SettlementDurability};
    let mut app = app();
    app.open_picker(vec![SessionId::new("s-1")]);
    app.sync_confirmation(Some((ConfirmationId::for_test(1), request("run_command"))));
    let effects = app.on_agent_event(&AgentEvent::OperationSettled {
        operation_id: OperationId::new("op-1"),
        status: OperationStatus::Succeeded,
        durability: SettlementDurability::Confirmed,
    });
    assert!(
        appended(&effects).is_empty(),
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
    assert_eq!(app.on_action(Action::CtrlC), vec![Effect::CancelActive]);
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
    run(&mut app, "second");

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
    let usage = philo_agent_runtime::TokenUsage {
        input_tokens: Some(5),
        output_tokens: Some(7),
        ..Default::default()
    };
    app.on_agent_event(&AgentEvent::ModelUsageUpdated {
        model_call_id: philo_agent_runtime::ModelCallId::new("m-1"),
        usage,
    });
    assert_eq!(app.status.usage, Some(usage));
}

#[test]
fn config_reload_applies_show_reasoning_and_reports_success() {
    use crate::api::types::ConfigReloadNotice;
    let mut app = app();
    let effects = app.on_action(Action::ConfigReload(ConfigReloadNotice::Applied {
        show_reasoning: false,
        verbose: false,
        context_window: Some(8_000),
        model_name: "model-b".to_owned(),
        runtime_pending: false,
        warnings: Vec::new(),
    }));
    assert!(!app.shows_reasoning());
    assert_eq!(app.status.model, "model-b");
    assert_eq!(app.status.context_window, Some(8_000));
    assert!(!app.status.config_reload_pending);
    assert_eq!(texts(&appended(&effects)), ["config reloaded"]);
}

#[test]
fn config_reload_pending_is_visible_and_not_repeated() {
    use crate::api::types::ConfigReloadNotice;
    let mut app = app();
    app.set_busy(true, 0);
    let first = app.on_action(Action::ConfigReload(ConfigReloadNotice::Pending));
    assert!(app.status.config_reload_pending);
    assert_eq!(texts(&appended(&first)), ["config: will apply after idle"]);
    let second = app.on_action(Action::ConfigReload(ConfigReloadNotice::Pending));
    assert!(second.is_empty());
}

#[test]
fn config_reload_failure_is_an_error_line() {
    use crate::api::types::ConfigReloadNotice;
    let mut app = app();
    let effects = app.on_action(Action::ConfigReload(ConfigReloadNotice::Failed {
        message: "config not reloaded: invalid TOML".to_owned(),
        clear_pending: false,
    }));
    let lines = appended(&effects);
    assert_eq!(lines[0].kind, LineKind::Error);
    assert!(lines[0].text.contains("invalid TOML"));
}
