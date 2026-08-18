//! Cross-layer attachment flow: queueing (`/image`, clipboard), the echo
//! the user sees, resolution on send, and the refusal that hands a message
//! back when a file cannot be read.

use crate::app::action::Action;
use crate::app::effect::Effect;
use crate::app::state::App;
use crate::app::status::StatusData;
use crate::app::transcript::{InfoLevel, TranscriptLine};
use crate::driver::media;

fn app() -> App {
    App::new(StatusData::new("model-a", "s-1", InfoLevel::Default), true)
}

fn submit(app: &mut App, text: &str) -> Vec<Effect> {
    for ch in text.chars() {
        app.on_action(Action::InsertChar(ch));
    }
    app.on_action(Action::Submit)
}

fn appended(effects: &[Effect]) -> Vec<String> {
    effects
        .iter()
        .flat_map(|effect| match effect {
            Effect::Append(lines) => lines.clone(),
            _ => Vec::new(),
        })
        .map(|line: TranscriptLine| line.text)
        .collect()
}

#[test]
fn attachment_echo_snapshot() {
    let mut app = app();
    let mut rendered: Vec<String> = Vec::new();

    rendered.extend(appended(&submit(&mut app, "/image shots/diagram.png")));
    rendered.extend(appended(&app.attach_image(
        "image/png".to_owned(),
        vec![0; 3_500],
        "clipboard image",
    )));
    rendered.push(format!(
        "[hint row] {}",
        app.attachments().summary().expect("attachments are queued")
    ));

    let effects = submit(&mut app, "compare these two");
    rendered.extend(appended(&effects));
    let Effect::PrepareSubmit {
        intent_id,
        text,
        attachments,
    } = effects[0].clone()
    else {
        panic!("the message carries its attachments");
    };

    // The first path cannot be read: the send is refused and the message
    // comes back with the attachments that did resolve.
    let resolved = media::resolve(attachments);
    rendered.extend(
        media::refusal_lines(&resolved.errors)
            .into_iter()
            .map(|line| line.text),
    );
    let _ = app.on_action(Action::SubmitMediaRefused {
        intent_id,
        kept: resolved.kept,
        errors: resolved.errors,
    });
    // Commit path is not taken; after refuse the draft is restored.
    assert_eq!(text, "compare these two");
    rendered.push(format!("[input] {}", app.input.text()));
    rendered.push(format!(
        "[hint row] {}",
        app.attachments()
            .summary()
            .expect("the readable one is still queued")
    ));

    crate::tests::assert_tui_snapshot!("attachment_echo", rendered.join("\n"));
}

#[test]
fn a_clipboard_failure_leaves_the_draft_alone() {
    let mut app = app();
    for ch in "half a thought".chars() {
        app.on_action(Action::InsertChar(ch));
    }
    let effects = app.clipboard_unavailable("this terminal has no reachable clipboard");
    assert_eq!(
        appended(&effects),
        [
            "no image on the clipboard (this terminal has no reachable clipboard); \
             attach a file with /image <path>"
        ]
    );
    assert_eq!(app.input.text(), "half a thought");
}
