//! Narrow-screen acceptance matrix (plan T7.1/T7.2).
//!
//! Five scenarios — idle, streaming, tools, busy, overlay — rendered at
//! 40/80/120 columns pin the responsive layouts, and a width walk pins the
//! contract's deterministic degradation ladders (tui.md §8, design §3.10):
//!
//! - top-left: lose `· esc cancel` first, then truncate the state word;
//! - bottom row: middle-ellipsize the path, then drop C%, R, ↑↓;
//!   `{ctx%}/{window}` and the two corners survive longest;
//! - top-right: drop effort, then provider, then truncate the model.

use philo_agent_service::{
    FrontendOperationEvent, FrontendTokenUsage, FrontendToolDisplay, FrontendToolResult,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::app::action::Action;
use crate::app::effect::Effect;
use crate::app::state::App;
use crate::app::status::StatusData;
use crate::app::text;
use crate::app::transcript::InfoLevel;
use crate::render::frame;

const WIDTHS: [u16; 3] = [40, 80, 120];
const HEIGHT: u16 = 24;
/// Below the supported floor nothing may panic; rows still fit.
const PROBE_WIDTHS: [u16; 4] = [40, 36, 30, 20];

fn dashboard() -> StatusData {
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
    status
}

fn app() -> App {
    App::new(dashboard(), true)
}

fn render(app: &App, width: u16) -> String {
    let backend = TestBackend::new(width, HEIGHT);
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

fn type_text(app: &mut App, s: &str) {
    for ch in s.chars() {
        app.on_action(Action::InsertChar(ch));
    }
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

fn freeze(app: &mut App, secs: u64) {
    app.run_state_mut()
        .freeze_elapsed(std::time::Duration::from_secs(secs));
}

/// A row must not overflow the terminal. TestBackend stores the continuation
/// cell of a wide grapheme as a plain space, so joining cells inflates the
/// measured width by one per wide character; the excess is cosmetic and
/// bounded by the wide-glyph count, so the check allows that slack.
fn cells_fit(rendered: &str, width: u16) {
    for row in rendered.lines() {
        let wide = row
            .chars()
            .filter(|c| text::width(c.to_string().as_str()) > 1)
            .count();
        let allowed = usize::from(width) + wide;
        assert!(
            text::width(row) <= allowed,
            "row exceeds {width} columns (wide-glyph slack {wide}): {row:?}"
        );
    }
}

fn matrix_render(build: fn() -> App) -> String {
    let mut sections = Vec::new();
    for width in WIDTHS {
        let rendered = render(&build(), width);
        cells_fit(&rendered, width);
        sections.push(format!("{width} columns\n{rendered}"));
    }
    // Below the supported floor: degradation without panic or overflow.
    for width in PROBE_WIDTHS {
        let rendered = render(&build(), width);
        cells_fit(&rendered, width);
    }
    sections.join("\n\n")
}

#[test]
fn idle_dashboard_across_the_matrix() {
    crate::tests::assert_tui_snapshot!("matrix_idle", matrix_render(app));
}

#[test]
fn streaming_screen_across_the_matrix() {
    fn build() -> App {
        let mut app = app();
        type_text(&mut app, "Find the homepage button and make it blue");
        let effects = app.on_action(Action::Submit);
        let Effect::PrepareSubmit { intent_id, .. } = &effects[0] else {
            panic!("expected a prepared submit");
        };
        app.on_action(Action::SubmitAccepted {
            intent_id: *intent_id,
            operation_id: "op-1".to_owned(),
        });
        apply(
            &mut app,
            &FrontendOperationEvent::ReasoningDelta {
                model_call_id: "call".to_owned(),
                text: "Looking at the middleware, I can see the refresh path skips \
                       expired tokens when a session cookie is still valid."
                    .to_owned(),
            },
        );
        apply(
            &mut app,
            &FrontendOperationEvent::TextDelta {
                delta: "The guard belongs right before the redirect.".to_owned(),
            },
        );
        assert!(app.flush_stream());
        // Pin the think span once the replay has sealed the block.
        app.cells.freeze_think(std::time::Duration::from_secs(8));
        app.set_busy(true);
        freeze(&mut app, 42);
        app
    }
    crate::tests::assert_tui_snapshot!("matrix_streaming", matrix_render(build));
}

#[test]
fn tool_cards_across_the_matrix() {
    fn build() -> App {
        let mut app = app();
        type_text(&mut app, "页码大于 5 时 /api/users 返回空列表，帮我修一下");
        let effects = app.on_action(Action::Submit);
        let Effect::PrepareSubmit { intent_id, .. } = &effects[0] else {
            panic!("expected a prepared submit");
        };
        app.on_action(Action::SubmitAccepted {
            intent_id: *intent_id,
            operation_id: "op-1".to_owned(),
        });

        complete(
            &mut app,
            0,
            "grep",
            r#"{"pattern":"page"}"#,
            FrontendToolResult::Success {
                content: "matches".to_owned(),
            },
            display(
                "src/routes/users.ts:12: if (page > limit)",
                &[
                    ("title", "Grep"),
                    ("body", "locs"),
                    ("subject", "\"page\""),
                    ("count", "1 search"),
                ],
            ),
        );
        complete(
            &mut app,
            1,
            "read",
            r#"{"paths":["src/routes/users.ts"]}"#,
            FrontendToolResult::Success {
                content: "contents".to_owned(),
            },
            display(
                "",
                &[
                    ("title", "Read"),
                    ("body", "none"),
                    ("subject", "src/routes/users.ts"),
                    ("subject", "src/routes/users.test.ts"),
                    ("count", "2 files"),
                ],
            ),
        );
        complete(
            &mut app,
            2,
            "edit",
            r#"{"path":"src/routes/users.ts"}"#,
            FrontendToolResult::Success {
                content: "replaced".to_owned(),
            },
            display(
                "@@ -6,2 +6,2 @@\n-const limit = page * 10;\n+const limit = Math.min(page * 10, 50);\n return paginate(page)",
                &[
                    ("title", "Edit"),
                    ("body", "diff"),
                    ("subject", "src/routes/users.ts"),
                    ("result", "Succeeded. File edited.  (+1 added, -1 removed)"),
                ],
            ),
        );
        complete(
            &mut app,
            3,
            "shell",
            r#"{"command":"pnpm test"}"#,
            FrontendToolResult::Success {
                content: "exit_code: 0".to_owned(),
            },
            display(
                "ok\npassed",
                &[
                    ("title", "Run"),
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
        app.run_state_mut().clear();
        app
    }
    crate::tests::assert_tui_snapshot!("matrix_tools", matrix_render(build));
}

#[test]
fn busy_running_state_across_the_matrix() {
    fn build() -> App {
        let mut app = app();
        type_text(&mut app, "Check the flaky test");
        let effects = app.on_action(Action::Submit);
        let Effect::PrepareSubmit { intent_id, .. } = &effects[0] else {
            panic!("expected a prepared submit");
        };
        app.on_action(Action::SubmitAccepted {
            intent_id: *intent_id,
            operation_id: "op-1".to_owned(),
        });
        apply(
            &mut app,
            &FrontendOperationEvent::ToolExecutionStarted {
                tool_batch_id: "batch-1".to_owned(),
                tool_call_id: "tool-1".to_owned(),
                index: 0,
                tool_name: "shell".to_owned(),
                arguments: r#"{"command":"pnpm test"}"#.to_owned(),
            },
        );
        app.set_busy(true);
        freeze(&mut app, 57);
        app
    }
    crate::tests::assert_tui_snapshot!("matrix_busy", matrix_render(build));
}

#[test]
fn confirmation_overlay_across_the_matrix() {
    fn build() -> App {
        let mut app = app();
        type_text(&mut app, "draft 中文");
        app.sync_confirmation(Some((
            7,
            "write workspace file".to_owned(),
            "path  src/auth/session.rs".to_owned(),
        )));
        app
    }
    crate::tests::assert_tui_snapshot!("matrix_overlay", matrix_render(build));
}

/// T7.2: walk the width downward and pin each ladder's internal order.
///
/// The three corners degrade independently against their own budgets, so
/// the contract chain holds per region; the bottom corners themselves are
/// the last things to disappear (§8).
#[test]
fn degradation_ladders_fire_in_contract_order() {
    fn build() -> App {
        let mut app = app();
        type_text(&mut app, "Find the homepage button");
        let effects = app.on_action(Action::Submit);
        let Effect::PrepareSubmit { intent_id, .. } = &effects[0] else {
            panic!("expected a prepared submit");
        };
        app.on_action(Action::SubmitAccepted {
            intent_id: *intent_id,
            operation_id: "op-1".to_owned(),
        });
        apply(
            &mut app,
            &FrontendOperationEvent::TextDelta {
                delta: "streaming".to_owned(),
            },
        );
        assert!(app.flush_stream());
        app.set_busy(true);
        freeze(&mut app, 42);
        app
    }

    // Largest width in the scan where the predicate is false (i.e. the
    // moment the feature disappears as the screen narrows).
    let drop_at = |probe: &dyn Fn(&App, u16) -> bool| -> u16 {
        let app = build();
        (20..=80)
            .rev()
            .find(|&width| !probe(&app, width))
            .unwrap_or(20)
    };

    let has =
        |needle: &'static str| move |app: &App, width: u16| render(app, width).contains(needle);

// Top-left badge (P2): the `(42s)` timer goes before the word truncates,
    // and the spinner+word pair never reaches the composer prompt. The old
    // `· esc cancel` tail is retired (D11), so it never appears.
    let timer_gone = drop_at(&|app, w| render(app, w).contains("(42s)"));
    let word_truncated = drop_at(&|app, w| render(app, w).contains("Writin"));
    assert!(
        timer_gone > word_truncated,
        "timer must drop ({timer_gone}) before the word truncates ({word_truncated})"
    );
    assert!(
        !render(&build(), 80).contains("esc cancel"),
        "D11: the esc tail is gone even at full width"
    );

    // Bottom row: path compacts, then C%, then R, then the arrows; the
    // context fraction outlives them all.
    let path_compact = drop_at(&has(r"D:\Code"));
    let cache_gone = drop_at(&has("C51"));
    let reasoning_gone = drop_at(&has("R14k"));
    let arrows_gone = drop_at(&has("↑11k"));
    let ctx_gone = drop_at(&has("500k"));
    assert!(
        path_compact > cache_gone && cache_gone > reasoning_gone && reasoning_gone > arrows_gone,
        "bottom ladder out of order: path {path_compact}, C {cache_gone}, \
         R {reasoning_gone}, arrows {arrows_gone}"
    );
    assert!(
        arrows_gone > ctx_gone,
        "the arrows drop ({arrows_gone}) before the ctx/window fraction ({ctx_gone})"
    );

// Top-right: effort drops, then the provider, then the model truncates
    // (§8 degradation order).
    let effort_gone = drop_at(&has("· high"));
    let model_gone = drop_at(&has("gpt-5.2"));
    assert!(
        effort_gone > model_gone,
        "model ladder out of order: effort {effort_gone}, model {model_gone}"
    );
    assert!(
        render(&build(), 80).contains("(openai)"),
        "the provider annotation shows at full width"
    );

    // The bottom corners survive at the supported floor (§8: last to go).
    let floor = render(&build(), frame::MIN_SUPPORTED_WIDTH);
    assert!(
        floor.contains(r"D:\…\Pi") && floor.contains("↑11k  ↓4.8k"),
        "both bottom corners stay visible at 40 columns: {floor}"
    );
}

// ---------------------------------------------------------------------------
// Prose typography (P3): the answer body across the width matrix.
// ---------------------------------------------------------------------------

const PROSE_ANSWER: &str = "\
# Prose blocks
## Middle rung
### Small rung
Body **bold**, *italic*, ~~struck~~ and `inline code`.
- bullet one
- bullet two
- [x] done deal
> quoted remark
see [docs](https://example.test/guide)
---
```rust
fn main() { println!(\"稳定\"); }
```
| plan | latency | implementation |
|---|---|---|
| fast | 2ms | cached |
| slow | 200ms | network every time |
Mixed 中文 and English long tail keeps wrapping at every width.";

fn stream_prose() -> App {
    let mut app = app();
    type_text(&mut app, "Show me every prose block");
    let effects = app.on_action(Action::Submit);
    let Effect::PrepareSubmit { intent_id, .. } = &effects[0] else {
        panic!("expected a prepared submit");
    };
    app.on_action(Action::SubmitAccepted {
        intent_id: *intent_id,
        operation_id: "op-1".to_owned(),
    });
    apply(
        &mut app,
        &FrontendOperationEvent::TextDelta {
            delta: PROSE_ANSWER.to_owned(),
        },
    );
    assert!(app.flush_stream());
    app.run_state_mut().clear();
    app
}

/// TP3.1: the full prose element zoo — heading ladder, inline styles,
/// lists, task list, quote, link, rule, fenced code, and a table — at
/// 40/80/120. The table's grid budget is 35 cells, so it flows at 40
/// (32-cell content column) and grids at 80/120; both shapes appear in
/// one snapshot. Probe widths below the floor exercise flow + wrap
/// without panic or overflow.
#[test]
fn prose_blocks_across_the_matrix() {
    crate::tests::assert_tui_snapshot!("matrix_prose", matrix_render(stream_prose));
}

/// TP2.2 end to end (prose v4 sizing): columns size to their widest cell
/// and squeeze before they flow — the grid only degrades to the dim pipe
/// flow when even one cell per column cannot fit (width < 4N+1).
#[test]
fn table_degradation_flows_below_the_grid_budget() {
    let app = stream_prose();
    // Natural widths: plan=4, latency=7, implementation=18 ("network every
    // time") → Σ29, frame overhead 10.

    // Content column 38, budget 28 ≥ 29? No — 28 < 29, still squeezed.
    // Step up to 48: with the right rail reserved the content column is
    // width-2·inset = 48-8-1 = 39; budget 29 = Σ → the table expands full
    // width, every cell on one line.
    let wide = render(&app, 48);
    assert!(
        wide.contains("│ plan │ latency │ implementation     │"),
        "natural widths fit the budget exactly (col pads to 18)"
    );
    assert!(wide.contains("network every time"), "no in-column wrap");

    // Content column 36, budget 26 < 29: the widest column squeezes to 8
    // and wraps at word boundaries; plan/latency keep single lines.
    let squeezed = render(&app, 44);
    assert!(squeezed.contains("│ plan │ latency │ implemen │"));
    assert!(squeezed.contains("│ network"), "word-boundary wrap inside the cell");
    assert!(!squeezed.contains("│ plan │ latency │ implementation │"));

    // Content column 2: not even one cell per column (budget 0 < 3) —
    // the whole run degrades to the dim pipe flow, frames included.
    // (Probed at the projection level: a 12-column viewport scrolls the
    // table out of the visible window.)
    let flowed = crate::app::prose::project_answer(
        "| plan | latency | implementation |\n|---|---|---|\n| fast | 2ms | cached |",
        12,
    );
    assert!(!flowed.iter().any(|row| row.text.contains('╭')));
    let joined: String = flowed.iter().map(|row| row.text.as_str()).collect();
    assert!(joined.contains("| plan") && joined.contains("implementation"));
}