//! Live tool-card runtime (v4.0 P3 §4/§5): the App's default-mode
//! interception turns started/progress/completed events into one cell that
//! is rewritten in place — a single running card or a concurrent tree — and
//! settles cancellation with the highest-priority `✗ cancelled`.

use philo_agent_service::{FrontendOperationEvent, FrontendToolDisplay, FrontendToolResult};

use crate::app::state::App;
use crate::app::status::StatusData;
use crate::app::tool_card::LIVE_TEXT_CHARS_MAX;
use crate::app::transcript::{InfoLevel, Tone};

fn app() -> App {
    App::new(
        StatusData::new("gpt-5.2", "session-中文", InfoLevel::Default),
        true,
    )
}

fn apply(app: &mut App, event: &FrontendOperationEvent) {
    let effects = app.on_operation_event(event);
    assert!(effects.is_empty(), "events write the store directly");
}

fn started(index: usize, name: &str, arguments: &str) -> FrontendOperationEvent {
    FrontendOperationEvent::ToolExecutionStarted {
        tool_batch_id: "batch-1".to_owned(),
        tool_call_id: format!("tool-{index}"),
        index,
        tool_name: name.to_owned(),
        arguments: arguments.to_owned(),
    }
}

fn progress(index: usize, tail: &str) -> FrontendOperationEvent {
    FrontendOperationEvent::ToolExecutionProgress {
        tool_batch_id: "batch-1".to_owned(),
        tool_call_id: format!("tool-{index}"),
        index,
        tail: tail.to_owned(),
    }
}

fn completed(
    index: usize,
    name: &str,
    result: FrontendToolResult,
    display: Option<FrontendToolDisplay>,
) -> FrontendOperationEvent {
    FrontendOperationEvent::ToolExecutionCompleted {
        tool_batch_id: "batch-1".to_owned(),
        tool_call_id: format!("tool-{index}"),
        index,
        tool_name: name.to_owned(),
        result,
        display,
    }
}

fn display(detail: &str, facts: &[(&str, &str)]) -> FrontendToolDisplay {
    FrontendToolDisplay {
        detail: detail.to_owned(),
        facts: facts
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
    }
}

/// A single announced tool rides exactly one running-card cell from its
/// `Started`; progress and completion rewrite that cell in place.
#[test]
fn single_card_starts_running_and_settles_in_place() {
    let mut app = app();
    apply(
        &mut app,
        &FrontendOperationEvent::ToolBatchRequested {
            tool_batch_id: "batch-1".to_owned(),
            call_count: 1,
        },
    );
    assert_eq!(app.cells.cells().len(), 0, "batch request alone draws nothing");

    apply(&mut app, &started(0, "read_file", r#"{"path":"src/main.rs"}"#));
    let cells = app.cells.cells();
    assert_eq!(cells.len(), 1, "Started creates the running card");
    assert!(app.animation_active(), "the running card drives animation");
    let card = &cells[0];
    assert_eq!(card.tone, Tone::Title);
    let header = card.header.as_ref().expect("running header");
    assert_eq!(header.bar.color, crate::app::transcript::SegColor::Yellow);
    assert_eq!(header.action.text, "read_file");
    assert_eq!(
        header.target.as_ref().map(|t| t.text.as_str()),
        Some("src/main.rs")
    );
    assert_eq!(
        header.status.color,
        crate::app::transcript::SegColor::Yellow,
        "the running status is the spinner"
    );

    // Progress rewrites the same cell, never appending a second one.
    apply(&mut app, &progress(0, "scanning…\n"));
    apply(&mut app, &progress(0, "more output"));
    let cells = app.cells.cells();
    assert_eq!(cells.len(), 1, "progress rewrites the live card in place");
    let body = cells[0].body.as_ref().expect("running body");
    assert!(
        body.lines.iter().any(|row| row[0].text == "scanning…"),
        "bounded output rides the live card: {body:?}"
    );

    apply(
        &mut app,
        &completed(
            0,
            "read_file",
            FrontendToolResult::Success {
                content: "fn main() {}".to_owned(),
            },
            Some(display(
                "",
                &[
                    ("title", "Read"),
                    ("body", "none"),
                    ("subject", "src/main.rs"),
                    ("count", "1 file"),
                ],
            )),
        ),
    );
    let cells = app.cells.cells();
    assert_eq!(cells.len(), 1, "completion settles the same cell");
    let card = &cells[0];
    let header = card.header.as_ref().expect("settled header");
    assert_eq!(header.bar.color, crate::app::transcript::SegColor::Green);
    assert_eq!(header.status.text, "✓ done");
}

/// The live output is capped at LIVE_TEXT_CHARS_MAX with a truncation
/// marker; everything past the cap is dropped.
#[test]
fn live_output_caps_at_the_character_budget() {
    let mut app = app();
    apply(&mut app, &started(0, "shell", r#"{"command":"seq"}"#));
    let chunk = "x".repeat(256);
    let mut fed = 0usize;
    while fed < LIVE_TEXT_CHARS_MAX + 1024 {
        apply(&mut app, &progress(0, &chunk));
        fed += chunk.len();
    }
    let cells = app.cells.cells();
    assert_eq!(cells.len(), 1);
    let body = cells[0].body.as_ref().expect("running body");
    let joined: String = body
        .lines
        .iter()
        .map(|row| row.iter().map(|seg| seg.text.as_str()).collect::<String>())
        .collect();
    let content_chars: usize = joined.chars().count();
    assert!(
        content_chars <= LIVE_TEXT_CHARS_MAX + 256,
        "the cap bounds the carried output: {content_chars}"
    );
    assert!(
        body.lines
            .iter()
            .any(|row| row.iter().any(|seg| seg.text.contains("… (truncated)"))),
        "the truncation marker appears past the cap"
    );
}

/// A batch larger than one is a tree cell created at the request, folded
/// as a whole; every settled child wears its own mini-card line and the
/// parent carries the total outcome.
#[test]
fn batch_tree_settles_all_children_and_parent() {
    let mut app = app();
    apply(
        &mut app,
        &FrontendOperationEvent::ToolBatchRequested {
            tool_batch_id: "batch-1".to_owned(),
            call_count: 3,
        },
    );
    let cells = app.cells.cells();
    assert_eq!(cells.len(), 1, "the tree cell exists from the request");
    assert!(
        app.animation_active(),
        "the running tree drives animation"
    );

    for index in 0..3 {
        apply(&mut app, &started(index, "grep", r#"{"pattern":"hit"}"#));
    }
    let cells = app.cells.cells();
    assert_eq!(cells.len(), 1, "children ride the one tree cell");
    let body = cells[0].body.as_ref().expect("tree body");
    assert_eq!(body.lines.len(), 3, "one child row per started tool");
    assert!(body.fold_all, "trees fold as a whole");
    assert_eq!(body.fold_count, 3);

    for index in 0..3 {
        apply(
            &mut app,
            &completed(
                index,
                "grep",
                FrontendToolResult::Success {
                    content: "ok".to_owned(),
                },
                Some(display(
                    "src/a.rs:1: hit",
                    &[
                        ("title", "Grep"),
                        ("body", "locs"),
                        ("subject", "\"hit\""),
                        ("matches_total", "1"),
                    ],
                )),
            ),
        );
    }
    let cells = app.cells.cells();
    assert_eq!(cells.len(), 1);
    let header = cells[0].header.as_ref().expect("parent header");
    assert_eq!(header.action.text, "Parallel Task (3 operations)");
    assert_eq!(header.status.text, "✓ done");
    assert_eq!(header.bar.color, crate::app::transcript::SegColor::Green);
    let body = cells[0].body.as_ref().expect("tree body");
    for row in &body.lines {
        let text = row.iter().map(|seg| seg.text.as_str()).collect::<String>();
        assert!(
            text.contains("grep") && text.contains("hit") && text.contains("✓ done"),
            "a settled child wears its mini-card: {text:?}"
        );
    }
}

/// Cancellation rewrites still-running slots to `✗ cancelled` — the
/// highest-priority settle that a later completion cannot overwrite.
#[test]
fn cancellation_settles_running_slots_to_failed() {
    let mut app = app();
    apply(
        &mut app,
        &FrontendOperationEvent::ToolBatchRequested {
            tool_batch_id: "batch-1".to_owned(),
            call_count: 2,
        },
    );
    apply(&mut app, &started(0, "read", r#"{"path":"src/a.rs"}"#));
    apply(&mut app, &started(1, "edit", r#"{"path":"src/b.rs"}"#));
    apply(
        &mut app,
        &completed(
            0,
            "read",
            FrontendToolResult::Success {
                content: "ok".to_owned(),
            },
            None,
        ),
    );

    apply(
        &mut app,
        &FrontendOperationEvent::CancellationRequested {
            operation_id: "op-1".to_owned(),
            reason: "User".to_owned(),
        },
    );
    let cells = app.cells.cells();
    let body = cells[0].body.as_ref().expect("tree body");
    let settled = body
        .lines
        .iter()
        .map(|row| row.iter().map(|seg| seg.text.as_str()).collect::<String>())
        .collect::<Vec<_>>();
    assert!(
        settled[0].contains("✓ done"),
        "the finished child keeps its result: {settled:?}"
    );
    assert!(
        settled[1].contains("✗ cancelled"),
        "the running child becomes cancelled: {settled:?}"
    );
    let parent = cells[0].header.as_ref().expect("parent header");
    assert_eq!(parent.status.text, "✗ failed");
    assert_eq!(parent.bar.color, crate::app::transcript::SegColor::Red);

    // A late completion must not overwrite the cancelled settle.
    apply(
        &mut app,
        &completed(
            1,
            "edit",
            FrontendToolResult::Success {
                content: "ok".to_owned(),
            },
            None,
        ),
    );
    let cells = app.cells.cells();
    let body = cells[0].body.as_ref().expect("tree body");
    let row_text: String = body.lines[1]
        .iter()
        .map(|seg| seg.text.as_str())
        .collect();
    assert!(
        row_text.contains("✗ cancelled"),
        "cancellation is sticky: {row_text:?}"
    );
}

/// A settled card with a body past the threshold folds by default and the
/// state API toggles it; the wrap cache follows both directions.
#[test]
fn completed_card_folds_and_toggles_via_the_state_api() {
    let mut app = app();
    let detail = (1..=20)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    apply(
        &mut app,
        &completed(
            0,
            "shell",
            FrontendToolResult::Success {
                content: "exit_code: 0\nok".to_owned(),
            },
            Some(display(
                &detail,
                &[
                    ("title", "Run"),
                    ("body", "output"),
                    ("subject", "seq"),
                    ("count", "1 command"),
                    ("exit_code", "0"),
                ],
            )),
        ),
    );
    let cells = app.cells.cells();
    assert_eq!(cells.len(), 2, "header cell plus the body cell");
    let body = cells[1].body.as_ref().expect("body cell");
    assert_eq!(body.lines.len(), 20);

    app.note_history_layout(80, 40);
    let folded_rows = app.history_slice(80, 40).rows.len();
    assert!(
        folded_rows < 20,
        "the body folds below its row count: {folded_rows}"
    );

    let folded = app.tool_card_collapsed_at(1);
    assert!(folded, "completion bodies fold by default");
    assert!(
        app.toggle_tool_card_fold(1),
        "the toggle flips a foldable card"
    );
    assert!(!app.tool_card_collapsed_at(1));
    let expanded_rows = app.history_slice(80, 40).rows.len();
    assert!(
        expanded_rows > folded_rows,
        "expanding reveals the hidden rows"
    );
    assert!(app.toggle_tool_card_fold(1));
    assert!(app.tool_card_collapsed_at(1));
}