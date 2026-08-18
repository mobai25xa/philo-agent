//! Aggregate acceptance across streaming, tools, compaction, cancellation,
//! and overlays: one presentation flow also covers Unicode.

use philo_agent_service::{FrontendOperationEvent, FrontendToolDisplay, FrontendToolResult};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::app::action::Action;
use crate::app::effect::Effect;
use crate::app::state::App;
use crate::app::status::StatusData;
use crate::app::text;
use crate::app::transcript::{InfoLevel, LineKind, TranscriptLine};
use crate::render::{composer, frame, markdown::MarkdownRenderer};

fn app() -> App {
    App::new(
        StatusData::new("model-m14", "session-m14", InfoLevel::Default),
        true,
    )
}

fn apply_event(app: &mut App, event: FrontendOperationEvent) {
    let effects = app.on_operation_event(&event);
    assert!(
        effects.is_empty(),
        "operation events write the transcript store directly: {effects:?}"
    );
}

fn answer_rows(cells: &[TranscriptLine]) -> Vec<String> {
    cells
        .iter()
        .filter(|line| line.kind == LineKind::Answer)
        .flat_map(|line| {
            let text = line.text.strip_suffix('\n').unwrap_or(line.text.as_str());
            if text.is_empty() {
                Vec::new()
            } else {
                text.split('\n').map(str::to_owned).collect::<Vec<_>>()
            }
        })
        .collect()
}

fn history_dump(cells: &[TranscriptLine]) -> String {
    cells
        .iter()
        .flat_map(|line| {
            let text = line.text.strip_suffix('\n').unwrap_or(line.text.as_str());
            let rows: Vec<&str> = if line.kind == LineKind::Answer {
                if text.is_empty() {
                    vec![""]
                } else {
                    text.split('\n').collect()
                }
            } else {
                vec![line.text.as_str()]
            };
            rows.into_iter()
                .map(|row| format!("{:?}: {}", line.kind, text::truncate(row, 72)))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render(app: &App, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let markdown = MarkdownRenderer::new();
    terminal
        .draw(|terminal_frame| frame::draw(terminal_frame, app, &markdown, false))
        .expect("test frame");
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
        .collect()
}

fn composer_row(app: &App, width: u16, height: u16) -> usize {
    let y = usize::from(frame::composer_y(height));
    let lines = render(app, width, height);
    let end = (y + 3).min(lines.len());
    assert!(
        lines[y..end]
            .iter()
            .any(|line| line.contains('›') || !line.trim().is_empty()),
        "composer slot is empty at row {y}: {lines:?}"
    );
    y
}

fn indexed_screen(app: &App, width: u16, height: u16) -> String {
    render(app, width, height)
        .into_iter()
        .enumerate()
        .map(|(row, line)| format!("{row:02}: {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_responsive_composer(app: &App, expected_rows: &[(u16, usize)]) {
    for &(width, expected_row) in expected_rows {
        assert_eq!(
            composer_row(app, width, frame::VIEWPORT_HEIGHT),
            expected_row,
            "transient state moved the {width}-column composer",
        );
        let viewport = composer::viewport(&app.input, usize::from(width), 3);
        assert!(viewport.cursor_x < usize::from(width));
        assert!(viewport.cursor_y < 3);
        assert!(
            viewport
                .rows
                .iter()
                .all(|row| text::width(row) <= usize::from(width)),
            "soft-wrapped input exceeded its composer width",
        );
    }
    assert!(
        render(app, 40, frame::MIN_SUPPORTED_HEIGHT)
            .iter()
            .any(|line| line.contains('›') || line.contains("draft") || line.contains("中文")),
        "the composer remains visible at the minimum supported height",
    );
}

#[test]
fn streaming_tool_compaction_and_cancel_form_one_stable_flow() {
    let mut app = app();
    let draft = "中文 e\u{301} 👩‍💻\nlong composer draft ".repeat(8);
    app.on_paste(&draft);
    let expected_rows =
        [40, 80, 120].map(|width| (width, composer_row(&app, width, frame::VIEWPORT_HEIGHT)));

    app.set_busy(true, 1);
    apply_event(
        &mut app,
        FrontendOperationEvent::ReasoningDelta {
            model_call_id: "model-call-1".to_owned(),
            text: "checking constraints\n".to_owned(),
        },
    );

    let long_line = format!("{} 中文 e\u{301} 👩‍💻", "0123456789".repeat(96));
    let first_answer = format!(
        "# Stable output\n{long_line}\n```rust\nfn main() {{ println!(\"稳定\"); }}\n```\n"
    );
    for ch in first_answer.chars() {
        apply_event(
            &mut app,
            FrontendOperationEvent::TextDelta {
                delta: ch.to_string(),
            },
        );
    }

    apply_event(
        &mut app,
        FrontendOperationEvent::ToolBatchRequested {
            tool_batch_id: "batch-1".to_owned(),
            call_count: 2,
        },
    );
    let before_started = app.cells.cells().len();
    apply_event(
        &mut app,
        FrontendOperationEvent::ToolExecutionStarted {
            tool_batch_id: "batch-1".to_owned(),
            tool_call_id: "tool-1".to_owned(),
            index: 0,
            tool_name: "read_file".to_owned(),
            arguments: r#"{"path":"src/中文.rs"}"#.to_owned(),
        },
    );
    assert_eq!(
        app.cells.cells().len(),
        before_started,
        "Started is Activity-only"
    );
    assert!(
        app.activity_view(40)
            .expect("tool activity")
            .text
            .contains("tool  read_file")
    );
    assert_responsive_composer(&app, &expected_rows);
    let tool_screen = indexed_screen(&app, 40, frame::VIEWPORT_HEIGHT);

    apply_event(
        &mut app,
        FrontendOperationEvent::ToolExecutionCompleted {
            tool_batch_id: "batch-1".to_owned(),
            tool_call_id: "tool-1".to_owned(),
            index: 0,
            tool_name: "read_file".to_owned(),
            result: FrontendToolResult::Success {
                content: format!("{} 完成", "result-".repeat(120)),
            },
            display: Some(FrontendToolDisplay {
                detail: "read completed\nfull display detail".to_owned(),
                facts: vec![("bytes".to_owned(), "840".to_owned())],
            }),
        },
    );
    let tool_lines = app
        .cells
        .cells()
        .iter()
        .filter(|line| line.kind == LineKind::Tool)
        .collect::<Vec<_>>();
    assert!(
        tool_lines[0].text.starts_with("• read_file"),
        "Completed keeps the fact summary: {:?}",
        tool_lines[0].text
    );
    assert_eq!(
        tool_lines[0].text, "• read_file  src/中文.rs",
        "old displays without verb/body become a header-only card"
    );
    assert_eq!(
        tool_lines.len(),
        1,
        "missing body fact means no body: {tool_lines:?}"
    );
    assert!(
        tool_lines
            .iter()
            .all(|line| !line.text.contains("full display detail")),
        "default cards must not dump display detail when body is missing"
    );
    assert!(
        tool_lines
            .iter()
            .all(|line| !line.text.contains("result-result-")),
        "default cards must not dump the model-facing body"
    );
    assert!(text::width(&tool_lines[0].text) <= 120);

    apply_event(&mut app, FrontendOperationEvent::ContextCompactionStarted);
    assert!(
        app.activity_view(40)
            .expect("compaction activity")
            .text
            .contains("compact")
    );
    apply_event(
        &mut app,
        FrontendOperationEvent::ContextCompactionCompleted {
            covers_up_to: "entry-42".to_owned(),
        },
    );

    apply_event(
        &mut app,
        FrontendOperationEvent::TextDelta {
            delta: "after tool".to_owned(),
        },
    );
    apply_event(
        &mut app,
        FrontendOperationEvent::CancellationRequested {
            operation_id: "op-1".to_owned(),
            reason: "User".to_owned(),
        },
    );
    apply_event(
        &mut app,
        FrontendOperationEvent::TextDelta {
            delta: " late-but-preserved".to_owned(),
        },
    );
    assert!(
        app.activity_view(40)
            .expect("cancellation activity")
            .text
            .contains("cancel  user"),
        "a late delta cannot replace the higher-priority cancellation",
    );
    assert_responsive_composer(&app, &expected_rows);
    let cancelling_screen = indexed_screen(&app, 40, frame::VIEWPORT_HEIGHT);

    apply_event(
        &mut app,
        FrontendOperationEvent::TurnCancelled {
            turn_id: "turn-1".to_owned(),
            reason: "User".to_owned(),
        },
    );
    apply_event(
        &mut app,
        FrontendOperationEvent::OperationSettled {
            operation_id: "op-1".to_owned(),
            session_id: "s-1".to_owned(),
            status: "Cancelled".to_owned(),
            durability: "Confirmed".to_owned(),
            session_revision: philo_agent_service::SettlementRevision::Unchanged,
        },
    );
    app.set_busy(false, 0);
    assert!(app.activity_view(40).is_none());
    assert!(!app.cells.has_open());

    let cells = app.cells.cells();
    let mut expected_answer_lines = first_answer
        .strip_suffix('\n')
        .expect("fixture ends in a newline")
        .split('\n')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    expected_answer_lines.extend(["after tool".to_owned(), " late-but-preserved".to_owned()]);
    assert_eq!(
        answer_rows(cells),
        expected_answer_lines,
        "all streamed text remains exact and ordered",
    );
    let late_text = cells
        .iter()
        .position(|line| line.kind == LineKind::Answer && line.text.contains(" late-but-preserved"))
        .expect("late text was committed");
    let cancelled = cells
        .iter()
        .position(|line| line.text == "turn cancelled (user)")
        .expect("cancellation fact was committed");
    assert!(
        late_text < cancelled,
        "a terminal cancellation fact cannot overtake accepted text",
    );

    app.open_picker(vec!["session-m14".to_owned(), "session-next".to_owned()]);
    assert_responsive_composer(&app, &expected_rows);
    app.on_action(Action::Escape);
    app.sync_confirmation(Some((
        14,
        "write workspace file".to_owned(),
        "src/中文.rs".to_owned(),
    )));
    assert_responsive_composer(&app, &expected_rows);
    assert_eq!(app.input.text(), draft, "overlays never consume the draft");
    let approval_screen = indexed_screen(&app, 40, frame::VIEWPORT_HEIGHT);

    let history = history_dump(app.cells.cells());
    crate::tests::assert_tui_snapshot!(
        "m14_complete_stability_flow",
        format!(
            "HISTORY\n{history}\n\nTOOL\n{tool_screen}\n\nCANCELLING\n{cancelling_screen}\n\nAPPROVAL\n{approval_screen}"
        )
    );
}

#[test]
fn animation_and_cancel_stay_live_without_runtime_handles() {
    let mut app = app();
    app.set_busy(true, 0);
    app.on_paste("草稿 e\u{301} 👩‍💻");
    let before_tick = app.activity_view(40).expect("waiting activity").text;
    assert!(app.on_tick());
    let after_tick = app.activity_view(40).expect("animated activity").text;
    assert_ne!(
        before_tick, after_tick,
        "spinner advances while the frontend waits"
    );
    assert_eq!(app.on_action(Action::Escape), [Effect::CancelActive]);
    assert_eq!(app.input.text(), "草稿 e\u{301} 👩‍💻");
}

#[tokio::test]
async fn start_test_service_client_accepts_attach_and_load() {
    let (service, client, _runtime) = philo_agent_service::testing::start_test_service();
    let lease = service
        .attach_frontend(
            philo_agent_service::FrontendInstanceId::new("tui-test"),
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .await
        .expect("attach");
    let loaded = client.try_command(philo_agent_service::FrontendCommand::LoadSession {
        session_id: "s-1".to_owned(),
    });
    assert!(matches!(
        loaded,
        philo_agent_service::CommandDispatch::Enqueued(_)
    ));
    service
        .detach_frontend(
            lease,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .await
        .expect("detach");
}
