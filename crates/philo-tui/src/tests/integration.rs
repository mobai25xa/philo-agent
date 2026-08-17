//! Aggregate acceptance across streaming, tools, compaction, cancellation,
//! and overlays: one presentation flow also covers Unicode and slow host work.

use std::sync::Arc;

use philo_agent_runtime::{
    AgentEvent, CancelReason, ModelCallId, OperationId, OperationStatus, SessionId,
    SettlementDurability, ToolBatchId, ToolCallId, ToolDisplay, ToolResult, TurnId, UserMessage,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::api::confirmation::{ConfirmationId, ConfirmationRequest};
use crate::api::host::TuiHost;
use crate::app::action::Action;
use crate::app::effect::Effect;
use crate::app::state::App;
use crate::app::status::StatusData;
use crate::app::text;
use crate::app::transcript::{InfoLevel, LineKind, TranscriptLine};
use crate::render::{composer, frame, markdown::MarkdownRenderer};
use crate::tests::support::{FakeHost, session_view};

fn app() -> App {
    App::new(
        StatusData::new("model-m14", "session-m14", InfoLevel::Default),
        true,
    )
}

fn apply_event(app: &mut App, event: AgentEvent, durable: &mut Vec<TranscriptLine>) {
    for effect in app.on_agent_event(&event) {
        match effect {
            Effect::Append(lines) => durable.extend(lines),
            other => panic!("agent event produced an unexpected effect: {other:?}"),
        }
    }
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
    render(app, width, height)
        .iter()
        .position(|line| line.contains("Message"))
        .expect("composer border remains visible")
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
        let viewport = composer::viewport(&app.input, usize::from(width - 2), 4);
        assert!(viewport.cursor_x < usize::from(width - 2));
        assert!(viewport.cursor_y < 4);
        assert!(
            viewport
                .rows
                .iter()
                .all(|row| text::width(row) <= usize::from(width - 2)),
            "soft-wrapped input exceeded its inner composer width",
        );
    }
    assert!(
        render(app, 40, frame::MIN_SUPPORTED_HEIGHT)
            .iter()
            .any(|line| line.contains("Message")),
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
    let mut durable = Vec::new();
    apply_event(
        &mut app,
        AgentEvent::ReasoningDelta {
            model_call_id: ModelCallId::new("model-call-1"),
            text: "checking constraints\n".to_owned(),
        },
        &mut durable,
    );

    let long_line = format!("{} 中文 e\u{301} 👩‍💻", "0123456789".repeat(96));
    let first_answer = format!(
        "# Stable output\n{long_line}\n```rust\nfn main() {{ println!(\"稳定\"); }}\n```\n"
    );
    for ch in first_answer.chars() {
        apply_event(
            &mut app,
            AgentEvent::TextDelta {
                delta: ch.to_string(),
            },
            &mut durable,
        );
    }

    apply_event(
        &mut app,
        AgentEvent::ToolBatchRequested {
            tool_batch_id: ToolBatchId::new("batch-1"),
            call_count: 2,
        },
        &mut durable,
    );
    let before_started = durable.len();
    apply_event(
        &mut app,
        AgentEvent::ToolExecutionStarted {
            tool_batch_id: ToolBatchId::new("batch-1"),
            tool_call_id: ToolCallId::new("tool-1"),
            index: 0,
            tool_name: "read_file".to_owned(),
            arguments: r#"{"path":"src/中文.rs"}"#.to_owned(),
        },
        &mut durable,
    );
    assert_eq!(durable.len(), before_started, "Started is Activity-only");
    assert!(
        app.activity_view(40)
            .expect("tool activity")
            .text
            .contains("tool 1/2  read_file")
    );
    assert_responsive_composer(&app, &expected_rows);
    let tool_screen = indexed_screen(&app, 40, frame::VIEWPORT_HEIGHT);

    apply_event(
        &mut app,
        AgentEvent::ToolExecutionCompleted {
            tool_batch_id: ToolBatchId::new("batch-1"),
            tool_call_id: ToolCallId::new("tool-1"),
            index: 0,
            tool_name: "read_file".to_owned(),
            result: ToolResult::success(format!("{} 完成", "result-".repeat(120))),
            display: Some(
                ToolDisplay::new("read completed\nfull display detail").with_fact("bytes", "840"),
            ),
        },
        &mut durable,
    );
    let tool_lines = durable
        .iter()
        .filter(|line| line.kind == LineKind::Tool)
        .collect::<Vec<_>>();
    assert_eq!(tool_lines.len(), 1, "Completed commits one default summary");
    assert!(text::width(&tool_lines[0].text) <= 120);

    apply_event(&mut app, AgentEvent::ContextCompactionStarted, &mut durable);
    assert!(
        app.activity_view(40)
            .expect("compaction activity")
            .text
            .contains("compact")
    );
    apply_event(
        &mut app,
        AgentEvent::ContextCompactionCompleted {
            covers_up_to: "entry-42".to_owned(),
        },
        &mut durable,
    );

    apply_event(
        &mut app,
        AgentEvent::TextDelta {
            delta: "after tool".to_owned(),
        },
        &mut durable,
    );
    apply_event(
        &mut app,
        AgentEvent::CancellationRequested {
            operation_id: OperationId::new("op-1"),
            reason: CancelReason::User,
        },
        &mut durable,
    );
    apply_event(
        &mut app,
        AgentEvent::TextDelta {
            delta: " late-but-preserved".to_owned(),
        },
        &mut durable,
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
        AgentEvent::TurnCancelled {
            turn_id: TurnId::new("turn-1"),
            reason: CancelReason::User,
        },
        &mut durable,
    );
    apply_event(
        &mut app,
        AgentEvent::OperationSettled {
            operation_id: OperationId::new("op-1"),
            status: OperationStatus::Cancelled,
            durability: SettlementDurability::Confirmed,
        },
        &mut durable,
    );
    app.set_busy(false, 0);
    assert!(app.activity_view(40).is_none());
    assert!(app.transcript.partial().is_none());

    let mut expected_answer_lines = first_answer
        .strip_suffix('\n')
        .expect("fixture ends in a newline")
        .split('\n')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    expected_answer_lines.extend(["after tool".to_owned(), " late-but-preserved".to_owned()]);
    assert_eq!(
        durable
            .iter()
            .filter(|line| line.kind == LineKind::Answer)
            .map(|line| line.text.clone())
            .collect::<Vec<_>>(),
        expected_answer_lines,
        "all streamed text remains exact and ordered",
    );
    let late_text = durable
        .iter()
        .position(|line| line.text == " late-but-preserved")
        .expect("late text was committed");
    let cancelled = durable
        .iter()
        .position(|line| line.text == "turn cancelled (user)")
        .expect("cancellation fact was committed");
    assert!(
        late_text < cancelled,
        "a terminal cancellation fact cannot overtake accepted text",
    );

    app.open_picker(vec![
        philo_session::SessionId::new("session-m14"),
        philo_session::SessionId::new("session-next"),
    ]);
    assert_responsive_composer(&app, &expected_rows);
    app.on_action(Action::Escape);
    app.sync_confirmation(Some((
        ConfirmationId::for_test(14),
        ConfirmationRequest {
            title: "write workspace file".to_owned(),
            body: "src/中文.rs".to_owned(),
        },
    )));
    assert_responsive_composer(&app, &expected_rows);
    assert_eq!(app.input.text(), draft, "overlays never consume the draft");
    let approval_screen = indexed_screen(&app, 40, frame::VIEWPORT_HEIGHT);

    let history = durable
        .iter()
        .map(|line| format!("{:?}: {}", line.kind, text::truncate(&line.text, 72)))
        .collect::<Vec<_>>()
        .join("\n");
    crate::tests::assert_tui_snapshot!(
        "m14_complete_stability_flow",
        format!(
            "HISTORY\n{history}\n\nTOOL\n{tool_screen}\n\nCANCELLING\n{cancelling_screen}\n\nAPPROVAL\n{approval_screen}"
        )
    );
}

#[tokio::test]
async fn slow_host_work_leaves_input_animation_cancel_and_completion_live() {
    let host = FakeHost::new();
    host.set_view("slow", session_view("slow"));
    let view_gate = host.delay_view("slow");
    let view_task = tokio::spawn({
        let host = Arc::clone(&host);
        async move {
            host.context_view(&philo_session::SessionId::new("slow"))
                .await
        }
    });
    yield_until(|| host.view_calls() == ["slow"]).await;

    let mut app = app();
    app.set_busy(true, 0);
    app.on_paste("草稿 e\u{301} 👩‍💻");
    let before_tick = app.activity_view(40).expect("waiting activity").text;
    assert!(app.on_tick());
    let after_tick = app.activity_view(40).expect("animated activity").text;
    assert_ne!(
        before_tick, after_tick,
        "spinner advances while Host is pending"
    );
    assert_eq!(app.on_action(Action::Escape), [Effect::CancelActive]);
    assert_eq!(app.input.text(), "草稿 e\u{301} 👩‍💻");

    view_gate.notify_one();
    let view = view_task
        .await
        .expect("view task joins")
        .expect("delayed view succeeds");
    assert_eq!(view.messages().len(), 4);

    host.delay_prompts();
    let prompt_task = tokio::spawn({
        let host = Arc::clone(&host);
        async move {
            host.prompt(SessionId::new("slow"), UserMessage::new("queued prompt"))
                .await
        }
    });
    yield_until(|| host.pending_prompts() == 1).await;
    app.on_action(Action::MoveLeft);
    app.on_action(Action::Backspace);
    assert!(
        app.on_tick(),
        "animation remains live during prompt admission"
    );
    prompt_task.abort();
    host.wait_for_prompt_cancellation().await;
    assert_eq!(host.prompt_cancellations(), 1);
    assert!(
        !app.input.text().is_empty(),
        "cancelling work preserves the draft"
    );
}

async fn yield_until(mut ready: impl FnMut() -> bool) {
    for _ in 0..256 {
        if ready() {
            return;
        }
        tokio::task::yield_now().await;
    }
    assert!(ready(), "background fixture did not become ready");
}
