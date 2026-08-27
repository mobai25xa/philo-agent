//! Style-aware baseline assertions over TestBackend cells.
//!
//! Text snapshots cannot see colors; this file pins the v4.0 brand-dark
//! palette at key coordinates so every token change here is deliberate and
//! reviewed.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};

use crate::app::state::App;
use crate::app::status::StatusData;
use crate::app::transcript::{InfoLevel, LineKind, Tone, TranscriptLine};
use crate::render::frame::draw;

const WIDTH: u16 = 80;
const HEIGHT: u16 = 16;

/// v4.0 brand-dark tokens: the structural set is constant (theme.rs); the
/// seven tunable foregrounds resolve through the startup default — the
/// Recommended de-glare row of `new-color.md` (§二).
const BASE_BG: Color = Color::Rgb(0x0D, 0x10, 0x16);
const FOOTER_BG: Color = Color::Rgb(0x0A, 0x0D, 0x12);
const ORANGE: Color = Color::Rgb(0xF2, 0x75, 0x21);
/// The recommended preset's damped bold accent (35% bold gain) — what the
/// bold+orange runs resolve to.
const ORANGE_BOLD: Color = Color::Rgb(166, 99, 54);
const GREEN: Color = Color::Rgb(0x51, 0xCD, 0x80);
const GRAY: Color = Color::Rgb(0x7E, 0x8C, 0x9E);
const DARK_GRAY: Color = Color::Rgb(0x5A, 0x6A, 0x7C);
const DIFF_ADD_BG: Color = Color::Rgb(0x15, 0x26, 0x1A);
const DIFF_DEL_BG: Color = Color::Rgb(0x26, 0x1A, 0x1A);
const MENU_ACTIVE_BG: Color = Color::Rgb(0x1A, 0x24, 0x33);
/// The recommended preset's damped bold white (35% bold gain) — bold
/// emphasis reads as a soft white, not a glare white.
const TEXT_BOLD: Color = Color::Rgb(201, 210, 221);
/// One-step-dimmer blue for a quick sanity probe (Unused today; kept
/// symbolically alongside the pinned table).
#[allow(dead_code)]
const BLUE: Color = Color::Rgb(0x62, 0x99, 0xEA);

struct Screen {
    rows: Vec<String>,
    styles: Vec<Vec<ratatui::style::Style>>,
}

fn app() -> App {
    App::new(
        StatusData::new("gpt-test", "session-中文", InfoLevel::Default),
        true,
    )
}

fn screen(app: &App) -> Screen {
    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| draw(frame, app, false))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut rows = Vec::new();
    let mut styles = Vec::new();
    for row in buffer.content.chunks(usize::from(WIDTH)) {
        rows.push(
            row.iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .trim_end()
                .to_owned(),
        );
        styles.push(row.iter().map(|cell| cell.style()).collect());
    }
    Screen { rows, styles }
}

impl Screen {
    fn row_containing(&self, needle: &str) -> usize {
        self.rows
            .iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("no row contains {needle:?}: {:#?}", self.rows))
    }

    /// The v4.0 P3 re-skinned think header row.
    fn think_row(&self) -> usize {
        self.rows
            .iter()
            .position(|row| row.contains("Thought") && row.contains("Space"))
            .unwrap_or_else(|| panic!("no think header: {:#?}", self.rows))
    }

    fn style_at(&self, row: usize, column: usize) -> ratatui::style::Style {
        self.styles[row][column]
    }

    /// Style of the first non-space symbol on the row.
    fn first_symbol_style(&self, row: usize) -> ratatui::style::Style {
        for (column, symbol) in self.rows[row].char_indices() {
            if symbol != ' ' {
                return self.style_at(row, column);
            }
        }
        panic!("row {row} has no symbols");
    }
}

fn fg(style: ratatui::style::Style) -> Color {
    style.fg.unwrap_or(Color::Reset)
}

fn bg(style: ratatui::style::Style) -> Color {
    style.bg.unwrap_or(Color::Reset)
}

#[test]
fn the_whole_canvas_is_base_bg_first() {
    // v4.0 paints every cell with the brand base before anything else; the
    // footer band then lays FOOTER_BG over the bottom rows, and the rail
    // column keeps its paint too.
    let app = app();
    let rendered = screen(&app);
    let footer_rows = 6usize; // idle band: rule + badge + box(3) + telemetry
    for (row, column, label, want) in [
        (0usize, 0usize, "top-left", BASE_BG),
        (0, usize::from(WIDTH) - 1, "top-right (scrollbar rail)", BASE_BG),
        (
            usize::from(HEIGHT) - 1,
            0,
            "bottom-left",
            FOOTER_BG,
        ),
        (
            usize::from(HEIGHT) - 1,
            usize::from(WIDTH) - 1,
            "bottom-right (reserved rail column)",
            BASE_BG,
        ),
        (
            usize::from(HEIGHT).saturating_sub(footer_rows + 1),
            usize::from(WIDTH) / 2,
            "just above the band",
            BASE_BG,
        ),
        (usize::from(HEIGHT) - footer_rows, usize::from(WIDTH) / 2, "band top", FOOTER_BG),
        (usize::from(HEIGHT) / 2, usize::from(WIDTH) / 2, "center", BASE_BG),
    ] {
        assert_eq!(
            bg(rendered.style_at(row, column)),
            want,
            "{label} rides {want:?}"
        );
    }
}

#[test]
fn baseline_idle_screen_pins_band_bar_and_placeholder_styles() {
    let app = app();
    let screen = screen(&app);

    let composer_row = screen.row_containing("Ask anything");
    // The whole footer band sits on FOOTER_BG now; the placeholder text
    // stays quiet dark gray.
    assert_eq!(
        fg(screen.style_at(composer_row, 8)),
        DARK_GRAY,
        "the placeholder copy stays quiet"
    );
}

#[test]
fn baseline_composer_corners_sit_on_the_canvas() {
    let app = app();
    let screen = screen(&app);

    // The placeholder centers on the middle line of the three-row box; the
    // badge row rides above the box top.
    let composer_row = screen.row_containing("Ask anything");
    let badge_row = composer_row - 2;
    assert!(
        screen.rows[badge_row].contains("● Ready"),
        "the badge rides between rule and box: {:?}",
        screen.rows[badge_row]
    );

    // Corner rows keep their content styling. The model name wears BLUE.
    let model_column = screen.rows[badge_row]
        .find("gpt-test")
        .expect("model name on the badge row");
    assert_eq!(
        fg(screen.style_at(badge_row, model_column)),
        BLUE,
        "the model name wears information blue"
    );

    // Telemetry row: identifiers gray, values yellow-bold, path green.
    let usage_row = composer_row + 2;
    let usage_column = screen.rows[usage_row]
        .find("↑")
        .expect("bottom-right usage corner");
    assert_eq!(fg(screen.style_at(usage_row, usage_column)), GRAY);
}

#[test]
fn baseline_transcript_cells_pin_role_colors() {
    let mut app = app();
    app.cells.push_closed([
        TranscriptLine {
            kind: LineKind::User,
            text: String::new(),
            tone: Tone::Plain,
            header: None,
            body: None,
        },
        TranscriptLine {
            kind: LineKind::User,
            text: "hello there".to_owned(),
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
        TranscriptLine {
            kind: LineKind::Answer,
            text: "answer body".to_owned(),
            tone: Tone::Plain,
            header: None,
            body: None,
        },
        TranscriptLine {
            kind: LineKind::Tool,
            text: "+added".to_owned(),
            tone: Tone::DiffIns,
            header: None,
            body: None,
        },
        TranscriptLine {
            kind: LineKind::Tool,
            text: "-removed".to_owned(),
            tone: Tone::DiffDel,
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
            text: "  quiet thought".to_owned(),
            tone: Tone::Plain,
            header: None,
            body: None,
        },
        TranscriptLine {
            kind: LineKind::Meta,
            text: "a note".to_owned(),
            tone: Tone::Plain,
            header: None,
            body: None,
        },
    ]);
    let initial = screen(&app);

    // The user message wears the green ❯ prefix and a bold white body on
    // the brand canvas.
    let user_row = initial.row_containing("hello there");
    let glyph_column = char_column(&initial.rows[user_row], "❯")
        .unwrap_or_else(|| panic!("prompt glyph leads the message"));
    let glyph_style = initial.style_at(user_row, glyph_column);
    assert_eq!(fg(glyph_style), GREEN, "the prompt glyph stays green");
    let text_column = char_column(&initial.rows[user_row], "hello")
        .unwrap_or_else(|| panic!("strip text not found"));
    assert_eq!(
        text_column,
        glyph_column + 2,
        "the body rides two cells past the glyph: {:?}",
        initial.rows[user_row]
    );
    let text_style = initial.style_at(user_row, text_column);
    assert_eq!(
        fg(text_style),
        TEXT_BOLD,
        "user text is damped bold white (demo weight 600)"
    );
    assert!(text_style.add_modifier.contains(Modifier::BOLD));

    let answer_row = initial.row_containing("answer body");
    let body_style = initial.first_symbol_style(answer_row);
    assert_eq!(
        body_style.fg,
        Some(Color::Reset),
        "answers stay terminal-default in the content column (P4 retunes prose)"
    );
    assert_eq!(bg(body_style), BASE_BG);

    let add_row = initial.row_containing("added");
    let add_style = initial.first_symbol_style(add_row);
    assert_eq!(
        fg(add_style),
        GREEN,
        "diff add foreground is helper green"
    );
    assert_eq!(
        bg(add_style),
        DIFF_ADD_BG,
        "diff add background pins to #15261A"
    );
    let del_row = initial.row_containing("removed");
    let del_style = initial.first_symbol_style(del_row);
    assert_eq!(fg(del_style), Color::Rgb(0xEB, 0x65, 0x65));
    assert_eq!(bg(del_style), DIFF_DEL_BG);

    let collapsed = screen(&app);
    let header_row = collapsed.think_row();
    // The v4.0 P3 think header: `▎` rail and the Space hint are dark gray,
    // the `Thought for` label is annotation gray.
    let bar_column = collapsed.rows[header_row].find('▎').expect("bar glyph");
    assert_eq!(fg(collapsed.style_at(header_row, bar_column)), DARK_GRAY);
    let label_column = collapsed.rows[header_row]
        .find("Thought")
        .expect("think label");
    assert_eq!(fg(collapsed.style_at(header_row, label_column)), GRAY);
    assert!(
        !collapsed
            .rows
            .iter()
            .any(|row| row.contains("quiet thought")),
        "sealed think blocks fold their body by default"
    );

    app.toggle_reasoning_block(6, 0);
    let expanded = screen(&app);
    let think_row = expanded.think_row();
    let bar_column = expanded.rows[think_row].find('▎').expect("bar glyph");
    assert_eq!(fg(expanded.style_at(think_row, bar_column)), DARK_GRAY);

    let body_row = expanded.row_containing("quiet thought");
    let body_style = expanded.first_symbol_style(body_row);
    assert_eq!(fg(body_style), GRAY);
    assert!(body_style.add_modifier.contains(Modifier::ITALIC));

    let note_row = expanded.row_containing("a note");
    assert_eq!(fg(expanded.first_symbol_style(note_row)), GRAY);
}

#[test]
fn baseline_prose_tokens_pin_answer_typography() {
    let mut app = app();
    app.cells.push_closed([TranscriptLine {
        kind: LineKind::Answer,
        text: [
            "# Heading one",
            "## Heading two",
            "- [x] done deal",
            "run `fast_path` now",
            "see [docs](https://e.co/guide)",
            "",
        ]
        .join("\n"),
        tone: Tone::Plain,
        header: None,
        body: None,
    }]);
    let screen = screen(&app);

    // H1: damped bold accent, bold, no underline (v4.0 P4 §1 + bold-gain
    // damping — a bold orange run steps down a rung from the plain accent).
    let h1_row = screen.row_containing("Heading one");
    let h1 = screen.first_symbol_style(h1_row);
    assert!(
        h1.add_modifier.contains(Modifier::BOLD)
            && !h1.add_modifier.contains(Modifier::UNDERLINED),
        "H1 keeps the top rung without the old underline: {h1:?}"
    );
    assert_eq!(fg(h1), ORANGE_BOLD, "H1 rides the damped bold accent");

    // H2: bold white (the second rung lifts past orange).
    let h2_row = screen.row_containing("Heading two");
    let h2 = screen.first_symbol_style(h2_row);
    assert!(h2.add_modifier.contains(Modifier::BOLD));
    assert!(!h2.add_modifier.contains(Modifier::UNDERLINED));
    assert_eq!(fg(h2), TEXT_BOLD, "H2 lifts to damped bold white");

    // Task list: damped bold bullet, plain accent checked box,
    // struck+gray done body.
    let task_row = screen.row_containing("done deal");
    let marker_column = char_column(&screen.rows[task_row], "•").expect("bullet");
    assert_eq!(
        fg(screen.style_at(task_row, marker_column)),
        ORANGE_BOLD,
        "the bullet rides the damped bold accent"
    );
    assert!(
        screen.style_at(task_row, marker_column).add_modifier.contains(Modifier::BOLD),
        "the bullet marker is bold"
    );
    let glyph_column = char_column(&screen.rows[task_row], "[x]").expect("task glyph");
    assert_eq!(
        &screen.rows[task_row][screen.rows[task_row].find("[x]").expect("task glyph")
            ..screen.rows[task_row].find("[x]").expect("task glyph") + 3],
        "[x]",
        "task glyphs stay literal (width safety)"
    );
    assert_eq!(
        fg(screen.style_at(task_row, glyph_column)),
        ORANGE,
        "the checked box lights up brand orange"
    );
    let body_column = char_column(&screen.rows[task_row], "done").expect("task body");
    let body_style = screen.style_at(task_row, body_column);
    assert!(body_style.add_modifier.contains(Modifier::CROSSED_OUT));
    assert_eq!(fg(body_style), GRAY);

    // Inline code: uniform green text, no background block.
    let pill_row = screen.row_containing("fast_path");
    let pill_column = char_column(&screen.rows[pill_row], "fast_path").expect("code span");
    let code_style = screen.style_at(pill_row, pill_column);
    assert_eq!(
        fg(code_style),
        GREEN,
        "code rides the uniform helper green without a palette"
    );
    assert_eq!(
        bg(code_style),
        BASE_BG,
        "code is font color only — no highlighter block"
    );

    // Links keep the blue convention with the underline.
    let link_row = screen.row_containing("docs");
    let link_column = char_column(&screen.rows[link_row], "docs").expect("link text");
    let link_style = screen.style_at(link_row, link_column);
    assert_eq!(fg(link_style), BLUE);
    assert!(link_style.add_modifier.contains(Modifier::UNDERLINED));
}

/// Display column of the first occurrence of `needle` in `row`.
fn char_column(row: &str, needle: &str) -> Option<usize> {
    let byte = row.find(needle)?;
    Some(row[..byte].chars().count())
}

#[test]
fn baseline_command_menu_pins_selection_and_usage_styles() {
    let mut app = app();
    for ch in "/s".chars() {
        app.on_action(crate::app::action::Action::InsertChar(ch));
    }
    let menu = screen(&app);

    let highlighted = menu.row_containing("/sessions");
    // The selected row leads with the orange edge bar (right after the
    // float's side rail) and the `▶` marker (P5 §3).
    let inset = usize::from(crate::render::theme::CONTENT_INSET);
    let edge_style = menu.style_at(highlighted, inset + 1);
    assert_eq!(
        menu.rows[highlighted].chars().nth(inset + 1),
        Some('▎'),
        "edge bar: {:?}",
        menu.rows[highlighted]
    );
    assert_eq!(fg(edge_style), ORANGE_BOLD, "edge bar is the damped accent");
    assert_eq!(
        bg(edge_style),
        MENU_ACTIVE_BG,
        "edge bar sits on the selected fill"
    );

    let marker_column = char_column(&menu.rows[highlighted], "▶").expect("selected marker");
    let highlight_style = menu.style_at(highlighted, marker_column);
    assert_eq!(
        fg(highlight_style),
        ORANGE_BOLD,
        "selected row text is the damped accent"
    );
    assert_eq!(
        bg(highlight_style),
        MENU_ACTIVE_BG,
        "selection paints the MENU_ACTIVE_BG fill"
    );
    assert!(highlight_style.add_modifier.contains(Modifier::BOLD));

    // The rounded float anchors at the content column's left edge; the
    // header row and its TRACK rule sit between the top border and the
    // highlighted row.
    let top_border = &menu.rows[highlighted - 3];
    let anchor_column = char_column(top_border, "╭").expect("rounded corner");
    let corner_column = char_column(top_border, "╮").expect("rounded corner");
    assert_eq!(
        anchor_column,
        inset,
        "the float hugs the shared content column"
    );
    assert!(corner_column > anchor_column);
    let header_row = &menu.rows[highlighted - 2];
    assert!(
        header_row.contains("Slash Commands") && header_row.contains("Tab complete"),
        "header row: {header_row:?}"
    );
    assert_eq!(
        bg(menu.style_at(highlighted, corner_column.saturating_sub(2))),
        MENU_ACTIVE_BG,
        "the fill runs through the right padding"
    );

    let plain = menu
        .rows
        .iter()
        .enumerate()
        .position(|(_, row)| row.contains("/status"))
        .expect("unselected candidate visible");
    let usage_column = char_column(&menu.rows[plain], "/status").expect("usage");
    assert_eq!(
        fg(menu.style_at(plain, usage_column)),
        crate::render::theme::primary().fg.expect("text default"),
        "unselected usage wears the default body tone"
    );
}

