//! Tool-card acceptance (redesign §3.3, plan T5.6).
//!
//! One screen pins the four card families from the design mock — Grep,
//! multi-path Read, Edit with its numbered diff gutter, and Run — plus a
//! cell dump pins the failure card (`↳ Failed. …`).

use philo_agent_service::{FrontendOperationEvent, FrontendToolDisplay, FrontendToolResult};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::app::action::Action;
use crate::app::state::App;
use crate::app::status::StatusData;
use crate::app::transcript::{InfoLevel, LineKind};
use crate::render::frame;

fn app() -> App {
    let status = StatusData::new("gpt-5.2", "session-中文", InfoLevel::Default);
    App::new(status, true)
}

fn render(app: &App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|f| frame::draw(f, app, false)).expect("draw");
    terminal
        .backend()
        .buffer()
        .content
        .chunks(usize::from(width))
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn apply(app: &mut App, event: &FrontendOperationEvent) {
    let effects = app.on_operation_event(event);
    assert!(effects.is_empty(), "events write the store directly");
}

fn display(detail: &str, facts: &[(&str, &str)]) -> Option<FrontendToolDisplay> {
    Some(FrontendToolDisplay {
        detail: detail.to_owned(),
        facts: facts
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
    })
}

fn complete(
    app: &mut App,
    index: usize,
    name: &str,
    _arguments: &str,
    result: FrontendToolResult,
    facts: Option<FrontendToolDisplay>,
) {
    apply(
        app,
        &FrontendOperationEvent::ToolExecutionCompleted {
            tool_batch_id: "batch-1".to_owned(),
            tool_call_id: format!("tool-{index}"),
            index,
            tool_name: name.to_owned(),
            result,
            display: facts,
        },
    );
}

#[test]
fn the_four_card_families_match_the_design_language() {
    let mut app = app();

    for ch in "Fix the empty list when page > 5".chars() {
        app.on_action(Action::InsertChar(ch));
    }
    let effects = app.on_action(Action::Submit);
    let crate::app::effect::Effect::PrepareSubmit { intent_id, .. } = &effects[0] else {
        panic!("expected a prepared submit");
    };
    app.on_action(Action::SubmitAccepted {
        intent_id: *intent_id,
        operation_id: "op-1".to_owned(),
    });
    apply(
        &mut app,
        &FrontendOperationEvent::ModelCallStarted {
            model_call_id: "call-1".to_owned(),
        },
    );

    // Grep 1 search — locs body capped at five rows.
    let locs = "src/routes/users.ts:12: if (page > limit)\n\
                src/routes/users.ts:14: return []\n\
                src/routes/users.test.ts:8: expect(users).toHaveLength(0)";
    complete(
        &mut app,
        0,
        "grep",
        r#"{"pattern":"page >","path":"src/routes"}"#,
        FrontendToolResult::Success {
            content: "matches…".to_owned(),
        },
        display(
            locs,
            &[
                ("title", "Grep"),
                ("verb", "Searched"),
                ("body", "locs"),
                ("subject", "\"page >\""),
                ("count", "1 search"),
                ("matches_total", "3"),
            ],
        ),
    );

    // Read 2 files — repeatable subjects under one header.
    complete(
        &mut app,
        1,
        "read",
        r#"{"paths":["src/routes/users.ts","src/routes/users.test.ts"]}"#,
        FrontendToolResult::Success {
            content: "file contents".to_owned(),
        },
        display(
            "",
            &[
                ("title", "Read"),
                ("verb", "Read"),
                ("body", "none"),
                ("subject", "src/routes/users.ts"),
                ("subject", "src/routes/users.test.ts"),
                ("count", "2 files"),
            ],
        ),
    );

    // Edit src/routes/users.ts — subject header, result row, numbered diff.
    complete(
        &mut app,
        2,
        "edit",
        r#"{"path":"src/routes/users.ts"}"#,
        FrontendToolResult::Success {
            content: "replaced".to_owned(),
        },
        display(
            "@@ -12,3 +12,4 @@\n-if (page > limit)\n-  return []\n+const size = Math.min(page, MAX_PAGE);\n+if (page > size)\n+  return clampPage(page)\n return users",
            &[
                ("title", "Edit"),
                ("verb", "Edited"),
                ("body", "diff"),
                ("subject", "src/routes/users.ts"),
                ("result", "Succeeded. File edited.  (+3 added, -2 removed)"),
            ],
        ),
    );

    // Run 1 command — output body under the result phrase.
    complete(
        &mut app,
        3,
        "shell",
        r#"{"command":"pnpm test"}"#,
        FrontendToolResult::Success {
            content: "exit_code: 0\nok".to_owned(),
        },
        display(
            "ok\npassed",
            &[
                ("title", "Run"),
                ("verb", "Ran"),
                ("body", "output"),
                ("subject", "pnpm test"),
                ("count", "1 command"),
                ("result", "exit 0 · 4.2s"),
            ],
        ),
    );

    apply(
        &mut app,
        &FrontendOperationEvent::TextDelta {
            delta: "All three paginated endpoints now clamp the page size.".to_owned(),
        },
    );
    assert!(app.flush_stream());
    app.set_busy(true);

    let rendered = render(&app, 80, 56);
    let rows: Vec<&str> = rendered.lines().collect();

    // Headers open their cards at the content column; no legacy bullets.
    for header in ["Grep 1 search", "Read 2 files", "Run 1 command"] {
        assert!(
            rows.iter()
                .any(|row| row.starts_with(&format!("    {header}"))),
            "{header} opens its card in the content column: {rendered}"
        );
    }
    assert!(
        !rendered.contains('•') && !rendered.contains('▸') && !rendered.contains('└'),
        "legacy glyph family is gone: {rendered}"
    );

    // Repeatable subjects align under the first ↳ row.
    let read_subject = rows
        .iter()
        .position(|row| row.contains("↳ src/routes/users.ts"))
        .expect("first read subject");
    assert!(
        rows[read_subject + 1].contains("src/routes/users.test.ts")
            && !rows[read_subject + 1].contains('↳'),
        "continuation subjects align without a second ↳: {rendered}"
    );

    // The Edit diff renders its gutter; the hunk header stays hidden.
    assert!(
        rows.iter()
            .any(|row| row.contains("12 | if (page > limit)")),
        "deleted lines carry their old number: {rendered}"
    );
    assert!(
        rows.iter().any(|row| row.contains("12 | const size")),
        "inserted lines carry their new number: {rendered}"
    );
    assert!(
        !rendered.contains("@@"),
        "the hunk header never renders: {rendered}"
    );

    crate::tests::assert_tui_snapshot!("m5_tool_cards", rendered);
}

#[test]
fn failures_keep_the_header_and_report_a_red_row() {
    let mut app = app();
    complete(
        &mut app,
        0,
        "edit",
        r#"{"path":"src/lib.rs"}"#,
        FrontendToolResult::Error {
            code: "not_unique".to_owned(),
            message: "old_string matches 3 locations".to_owned(),
        },
        None,
    );
    let dump = app
        .cells
        .cells()
        .iter()
        .filter(|cell| cell.kind == LineKind::Tool)
        .map(|cell| format!("{:?}: {}", cell.tone, cell.text))
        .collect::<Vec<_>>()
        .join("\n");
    crate::tests::assert_tui_snapshot!("m5_failure_card", dump);
}
