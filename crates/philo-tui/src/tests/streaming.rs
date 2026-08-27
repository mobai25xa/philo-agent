//! Streaming-transcript acceptance (redesign §3.2/§3.4, plan T4.6).
//!
//! One screen pins the §3.2 writing state — user strip with its column-0
//! bar, the sealed think header carrying its wall-clock span, the bare
//! answer column, and the composer's running state word. A cell dump pins
//! the §3.4 failure/retry/cancel facts through settlement.

use philo_agent_service::{FrontendFailure, FrontendOperationEvent, FrontendTokenUsage};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::app::action::Action;
use crate::app::state::App;
use crate::app::status::StatusData;
use crate::app::transcript::InfoLevel;
use crate::render::frame;

fn dashboard_app() -> App {
    let mut status = StatusData::new("gpt-5.2", "session-中文", InfoLevel::Default);
    status.provider = Some("openai".to_owned());
    status.effort = Some("high".to_owned());
    status.workspace_root = r"D:\Code\Zed\Year2026\Jul0706\Pi".to_owned();
    status.usage = Some(FrontendTokenUsage {
        input_tokens: Some(11_000),
        output_tokens: Some(4_800),
        cache_read_tokens: Some(5_640),
        reasoning_tokens: Some(14_000),
        ..FrontendTokenUsage::default()
    });
    status.context_window = Some(500_000);
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

#[test]
fn the_writing_screen_matches_the_design_language() {
    let mut app = dashboard_app();

    type_text(&mut app, "Find the homepage button and make it blue");
    let effects = app.on_action(Action::Submit);
    let crate::app::effect::Effect::PrepareSubmit { intent_id, .. } = &effects[0] else {
        panic!("expected a prepared submit");
    };
    app.on_action(crate::app::action::Action::SubmitAccepted {
        intent_id: *intent_id,
        operation_id: "op-1".to_owned(),
    });

    apply(
        &mut app,
        &FrontendOperationEvent::ReasoningDelta {
            model_call_id: "call-1".to_owned(),
            text: "the refresh path matters here\n".to_owned(),
        },
    );
    apply(
        &mut app,
        &FrontendOperationEvent::TextDelta {
            delta: "Looking at the middleware, I can see the refresh path skips \
                    expired tokens when a session cookie is still valid."
                .to_owned(),
        },
    );
    // Paced delivery holds both deltas; flushing replays them through the
    // same reducer paths. Pin the think span afterwards so the sealed
    // header is deterministic.
    assert!(app.flush_stream());
    app.cells.freeze_think(std::time::Duration::from_secs(8));

    app.set_busy(true);
    app.run_state_mut()
        .freeze_elapsed(std::time::Duration::from_secs(42));

    let rendered = render(&app, 80, 24);
    let rows: Vec<&str> = rendered.lines().collect();

    // User strip: the input band (content column ±1) with the bar on its
    // first column (col 3) and text two cells in (col 5).
    let strip = rows
        .iter()
        .position(|row| row.contains("Find the homepage"))
        .expect("strip row");
    assert!(
        rows[strip].contains("❯ Find the homepage"),
        "strip row leads with the prompt prefix: {:?}",
        rows[strip]
    );
    assert_eq!(
        rows.get(strip - 1).map(|row| row.trim().trim_end_matches(['│', '█']).trim()),
        Some(""),
        "the separator above the strip stays bare"
    );

    // Sealed think header carries the frozen first-to-last delta span.
    assert!(
        rows.iter().any(|row| {
            let row = row.trim().trim_end_matches(['│', '█']).trim();
            row.contains("Thought for 8s") && row.contains("Space")
        }),
        "the timed think header renders: {rendered}"
    );
    assert!(
        rows[..rows.len().saturating_sub(6)]
            .iter()
            .all(|row| row.trim_end_matches(['│', '█']).trim().chars().all(|c| c != '│')),
        "the folded body stays hidden: {rendered}"
    );

    // Answer streams bare in the content column; the state word runs.
    assert!(
        rows.iter()
            .any(|row| row.contains("Looking at the middleware")),
        "the open answer renders: {rendered}"
    );
    assert!(
        rows.iter().any(|row| row.contains("Writing (42s)")),
        "top-left carries the running word outside the box: {rendered}"
    );
    assert!(
        rows.iter().any(|row| row.contains("gpt-5.2 · high")),
        "model corner survives: {rendered}"
    );

    crate::tests::assert_tui_snapshot!("m4_writing_strip", rendered);
}

/// Production-path proof for plan P0: a fence streamed through `TextDelta`
/// must paint its `│ ` gutter and highlighted body in the live view — the
/// old preview path never advanced block state, so real runs lost both.
#[test]
fn streamed_code_fences_paint_their_gutter_and_highlight() {
    let mut app = dashboard_app();
    type_text(&mut app, "show me rust");
    let effects = app.on_action(Action::Submit);
    let crate::app::effect::Effect::PrepareSubmit { intent_id, .. } = &effects[0] else {
        panic!("expected a prepared submit");
    };
    app.on_action(Action::SubmitAccepted {
        intent_id: *intent_id,
        operation_id: "op-fence".to_owned(),
    });

    for delta in [
        "Here is the entry point:\n",
        "```rust\n",
        "let answer = 42;\n",
        "```\n",
        "Done.",
    ] {
        apply(
            &mut app,
            &FrontendOperationEvent::TextDelta {
                delta: delta.to_owned(),
            },
        );
    }
    assert!(app.flush_stream());

    let width = 80_u16;
    let backend = TestBackend::new(width, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| frame::draw(f, &app, false))
        .expect("draw");

    let columns = usize::from(width);
    let rows: Vec<String> = terminal
        .backend()
        .buffer()
        .content
        .chunks(columns)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect();
    let body_row = rows
        .iter()
        .position(|row| row.contains("let answer = 42;"))
        .expect("fenced body renders");
    assert!(
        rows[body_row].trim_start().starts_with("1 │ let answer"),
        "a streamed fenced body paints the numbered gutter: {}",
        rows.join("\n")
    );

    // The highlight itself rides syntect: body cells beyond the gutter wear
    // an explicit foreground, where plain answer prose leaves it unset.
    let offset = body_row * columns;
    let highlighted = terminal.backend().buffer().content[offset..offset + columns]
        .iter()
        .skip_while(|cell| cell.symbol() != "l")
        .any(|cell| cell.style().fg.is_some());
    assert!(
        highlighted,
        "syntect colours the fenced body in the live view"
    );
}

fn type_text(app: &mut App, text: &str) {
    for ch in text.chars() {
        app.on_action(Action::InsertChar(ch));
    }
}

#[test]
fn failure_retry_and_cancel_lines_land_in_order() {
    let failure = || FrontendFailure {
        code: "TIMEOUT".to_owned(),
        domain: "network".to_owned(),
        stage: "model_call".to_owned(),
        retry: "safe".to_owned(),
        summary: "upstream timeout after 30s".to_owned(),
        diagnostic: String::new(),
    };

    // Retry: two tiers, and the corner words it.
    let mut app = dashboard_app();
    app.set_busy(true);
    apply(
        &mut app,
        &FrontendOperationEvent::ModelCallStarted {
            model_call_id: "call-1".to_owned(),
        },
    );
    apply(
        &mut app,
        &FrontendOperationEvent::ModelRetryScheduled {
            model_call_id: "call-1".to_owned(),
            attempt: 1,
            max_retries: 3,
            delay_ms: 2_000,
            failure: failure(),
        },
    );
    let retry_word = app.run_state_corner(40).expect("retrying corner");
    assert!(retry_word.word.starts_with("Retrying"));

    // Exhausted retries: the three-tier terminal failure, then settlement.
    apply(
        &mut app,
        &FrontendOperationEvent::TurnFailed {
            turn_id: "turn-1".to_owned(),
            failure: failure(),
        },
    );
    app.run_state_mut()
        .freeze_elapsed(std::time::Duration::from_secs(30));
    apply(
        &mut app,
        &FrontendOperationEvent::OperationSettled {
            operation_id: "op-1".to_owned(),
            session_id: "s-1".to_owned(),
            status: "Failed".to_owned(),
            durability: "Confirmed".to_owned(),
            session_revision: philo_agent_service::SettlementRevision::Unchanged,
        },
    );
    let failed_dump = cell_dump(&app);

    // User cancellation: one settlement meta line with reason and duration.
    let mut app = dashboard_app();
    app.set_busy(true);
    apply(
        &mut app,
        &FrontendOperationEvent::CancellationRequested {
            operation_id: "op-1".to_owned(),
            reason: "User".to_owned(),
        },
    );
    apply(
        &mut app,
        &FrontendOperationEvent::TurnCancelled {
            turn_id: "turn-1".to_owned(),
            reason: "User".to_owned(),
        },
    );
    app.run_state_mut()
        .freeze_elapsed(std::time::Duration::from_secs(12));
    apply(
        &mut app,
        &FrontendOperationEvent::OperationSettled {
            operation_id: "op-1".to_owned(),
            session_id: "s-1".to_owned(),
            status: "Cancelled".to_owned(),
            durability: "Confirmed".to_owned(),
            session_revision: philo_agent_service::SettlementRevision::Unchanged,
        },
    );
    let cancelled_dump = cell_dump(&app);

    crate::tests::assert_tui_snapshot!(
        "m4_failure_cancel_lines",
        format!(
            "RETRY WORD\n{retry_word:?}\n\nFAILED\n{failed_dump}\n\nCANCELLED\n{cancelled_dump}"
        )
    );
}

fn cell_dump(app: &App) -> String {
    app.cells
        .cells()
        .iter()
        .map(|cell| format!("{:?}: {}", cell.kind, cell.text))
        .collect::<Vec<_>>()
        .join("\n")
}

fn indexed(screen: &str) -> String {
    screen
        .lines()
        .enumerate()
        .map(|(i, row)| format!("{i:02}|{row}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// v4.1 layout policy (stream anchors retired): sparse content lays out
/// from the band top; overflow pushes the tail in from the bottom edge,
/// and settlement never moves the view.
#[test]
fn streaming_lays_out_from_the_top_then_follows_the_bottom() {
    let mut app = dashboard_app();
    type_text(&mut app, "lift me");
    let effects = app.on_action(Action::Submit);
    let crate::app::effect::Effect::PrepareSubmit { intent_id, .. } = &effects[0] else {
        panic!("expected a prepared submit");
    };
    app.on_action(crate::app::action::Action::SubmitAccepted {
        intent_id: *intent_id,
        operation_id: "op-top".to_owned(),
    });
    apply(
        &mut app,
        &FrontendOperationEvent::OperationStarted {
            operation_id: "op-top".to_owned(),
        },
    );

    // Sparse content: the user strip (blank + `❯ lift me` + blank) rides
    // the band top; the first streamed row lands right below it.
    apply(
        &mut app,
        &FrontendOperationEvent::TextDelta {
            delta: "first words".to_owned(),
        },
    );
    assert!(app.flush_stream());
    let sparse = render(&app, 80, 40);
    let rows: Vec<&str> = sparse.lines().collect();
    assert!(rows[1].contains("lift me"), "user strip at the top:\n{sparse}");
    assert_eq!(
        rows[3].trim().trim_end_matches(['│', '█']).trim(),
        "first words",
        "sparse content starts at the band top:\n{sparse}"
    );
    assert!(
        rows[30].trim().trim_end_matches(['│', '█']).trim().is_empty(),
        "the band below sparse content stays blank:\n{sparse}"
    );

    // Growth: each new wrapped row lands below the previous one.
    apply(
        &mut app,
        &FrontendOperationEvent::TextDelta {
            delta: "\nand more".to_owned(),
        },
    );
    assert!(app.flush_stream());
    let grown = render(&app, 80, 40);
    let rows: Vec<&str> = grown.lines().collect();
    assert_eq!(
        rows[4].trim().trim_end_matches(['│', '█']).trim(),
        "and more",
        "new rows descend from the top while sparse:\n{}",
        indexed(&grown)
    );

    // Flood: once content overflows, the tail hugs the band floor (the
    // composer's separator row on this screen is row 34).
    apply(
        &mut app,
        &FrontendOperationEvent::TextDelta {
            delta: "\nmore".repeat(30),
        },
    );
    assert!(app.flush_stream());
    let flooded = render(&app, 80, 40);
    let rows: Vec<&str> = flooded.lines().collect();
    assert!(
        rows[33].contains("more"),
        "the overflowing tail hugs the band floor:\n{flooded}"
    );

    // Settlement adds one line at the tail; the overflowing view follows
    // the bottom, so only the top row scrolls away — the floor stays put.
    app.run_state_mut()
        .freeze_elapsed(std::time::Duration::from_secs(7));
    apply(
        &mut app,
        &FrontendOperationEvent::OperationSettled {
            operation_id: "op-top".to_owned(),
            session_id: "session-中文".to_owned(),
            status: "Succeeded".to_owned(),
            durability: "Confirmed".to_owned(),
            session_revision: philo_agent_service::SettlementRevision::Unchanged,
        },
    );
    app.set_busy(false);
    let settled = render(&app, 80, 40);
    let rows: Vec<&str> = settled.lines().collect();
    assert!(
        rows[33].contains("turn finished"),
        "the settlement line rides the band floor:\n{settled}"
    );
    assert!(
        !rows[1].contains("lift me"),
        "the pushed-out top row scrolled away, bottom stayed anchored:\n{settled}"
    );
}
