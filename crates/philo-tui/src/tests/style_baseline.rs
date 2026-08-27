//! Style-aware baseline assertions over TestBackend cells.
//!
//! Text snapshots cannot see colors; this file pins the current palette at
//! key coordinates so every token change here is deliberate and reviewed.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};

use crate::app::state::App;
use crate::app::status::StatusData;
use crate::app::transcript::{InfoLevel, LineKind, Tone, TranscriptLine};
use crate::render::frame::draw;

const WIDTH: u16 = 80;
const HEIGHT: u16 = 16;

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
fn baseline_idle_screen_pins_band_bar_and_placeholder_styles() {
    let app = app();
    let screen = screen(&app);

    assert!(
        !screen.rows.iter().any(|row| row.contains("───")),
        "idle separator rule stays deleted"
    );

    let composer_row = screen.row_containing("Ask anything");
    // The band spans the input band (content column ±1): native background
    // outside it, surface wash inside.
    assert_eq!(
        bg(screen.style_at(composer_row, 0)),
        Color::Reset,
        "the band no longer bleeds past the input band"
    );
    assert_eq!(
        bg(screen.style_at(composer_row, usize::from(WIDTH) - 1)),
        Color::Reset,
        "the band no longer bleeds past the input band"
    );
    assert_eq!(
        bg(screen.style_at(composer_row, 8)),
        Color::Indexed(236),
        "composer band falls back to stable indexed gray without a palette"
    );
    assert_eq!(
        bg(screen.style_at(composer_row, usize::from(WIDTH) - 5)),
        Color::Indexed(236),
        "composer band reaches the input band's right edge"
    );
    // The bar rides the band's first column.
    let bar_column = usize::from(crate::render::theme::BAND_INSET);
    assert_eq!(screen.rows[composer_row].chars().nth(bar_column), Some('▌'));
    let bar_style = screen.style_at(composer_row, bar_column);
    assert_eq!(
        fg(bar_style),
        Color::Rgb(222, 137, 72),
        "the band bar keeps the accent"
    );
    assert_eq!(
        bg(bar_style),
        Color::Indexed(236),
        "the bar rides the band surface"
    );
    let gutter_column = screen.rows[composer_row]
        .find("Ask anything")
        .unwrap_or_else(|| panic!("placeholder text not found"));
    assert_eq!(
        fg(screen.style_at(composer_row, gutter_column)),
        Color::DarkGray,
        "placeholder copy stays quiet"
    );
}

#[test]
fn baseline_composer_corners_sit_outside_the_input_box() {
    let app = app();
    let screen = screen(&app);

    // The placeholder centers on the middle line of the three-row box.
    let composer_row = screen.row_containing("Ask anything");
    // The corner rows hug the box directly (v2.3 compaction): the band's
    // outer rows still carry the wash, the corner rows themselves stay on
    // the native background — no blank rows in between.
    assert_eq!(
        bg(screen.style_at(composer_row - 1, 8)),
        Color::Indexed(236),
        "the band's top row rides the surface"
    );
    assert_ne!(
        bg(screen.style_at(composer_row - 2, 8)),
        Color::Indexed(236),
        "the top corner row hugs the box on native background"
    );
    assert_ne!(
        bg(screen.style_at(composer_row + 2, 8)),
        Color::Indexed(236),
        "the bottom corner row hugs the box on native background"
    );

    // Corner rows sit on the native background. The model name itself
    // keeps the primary token; only its dim chrome brightens to gray.
    let model_row = composer_row - 2;
    let model_column = screen.rows[model_row]
        .find("gpt-test")
        .expect("top-right model corner");
    assert_eq!(
        bg(screen.style_at(model_row, model_column)),
        Color::Reset,
        "the top-right corner sits on the native background"
    );
    assert_eq!(
        fg(screen.style_at(model_row, model_column)),
        Color::Reset,
        "the model name keeps the primary weight"
    );

    let usage_row = composer_row + 2;
    let usage_column = screen.rows[usage_row]
        .find("↑")
        .expect("bottom-right usage corner");
    assert_eq!(bg(screen.style_at(usage_row, usage_column)), Color::Reset);
    assert_eq!(fg(screen.style_at(usage_row, usage_column)), Color::Gray);
}

#[test]
fn baseline_transcript_cells_pin_role_colors() {
    let mut app = app();
    app.cells.push_closed([
        TranscriptLine {
            kind: LineKind::User,
            text: String::new(),
            tone: Tone::Plain,
        },
        TranscriptLine {
            kind: LineKind::User,
            text: "hello there".to_owned(),
            tone: Tone::Plain,
        },
        TranscriptLine {
            kind: LineKind::User,
            text: String::new(),
            tone: Tone::Plain,
        },
        TranscriptLine {
            kind: LineKind::Answer,
            text: "answer body".to_owned(),
            tone: Tone::Plain,
        },
        TranscriptLine {
            kind: LineKind::Tool,
            text: "+added".to_owned(),
            tone: Tone::DiffIns,
        },
        TranscriptLine {
            kind: LineKind::Tool,
            text: "-removed".to_owned(),
            tone: Tone::DiffDel,
        },
        TranscriptLine {
            kind: LineKind::Reasoning,
            text: "think".to_owned(),
            tone: Tone::Plain,
        },
        TranscriptLine {
            kind: LineKind::Reasoning,
            text: "  quiet thought".to_owned(),
            tone: Tone::Plain,
        },
        TranscriptLine {
            kind: LineKind::Meta,
            text: "a note".to_owned(),
            tone: Tone::Plain,
        },
    ]);
    let initial = screen(&app);

    // The user strip paints the input band (content column ±1) with an
    // accent bar on its first column; blank separator rows keep the
    // terminal background.
    let user_row = initial.row_containing("hello there");
    let bar_column = usize::from(crate::render::theme::BAND_INSET);
    let bar_style = initial.style_at(user_row, bar_column);
    assert_eq!(initial.rows[user_row].chars().nth(bar_column), Some('▌'));
    assert_eq!(
        fg(bar_style),
        Color::Rgb(222, 137, 72),
        "the strip bar keeps the accent"
    );
    assert_eq!(
        bg(bar_style),
        Color::Indexed(236),
        "the strip rides the band surface"
    );
    assert_eq!(
        bg(initial.style_at(user_row, 0)),
        Color::Reset,
        "the strip no longer bleeds past the input band"
    );
    let text_column = char_column(&initial.rows[user_row], "hello")
        .unwrap_or_else(|| panic!("strip text not found"));
    assert_eq!(
        text_column,
        usize::from(crate::render::theme::BAND_INSET + crate::render::theme::STRIP_TEXT_INSET),
        "strip text rides the bar-gap rhythm: {:?}",
        initial.rows[user_row]
    );
    let text_style = initial.style_at(user_row, text_column);
    assert_eq!(
        fg(text_style),
        Color::Reset,
        "user text wears the primary token"
    );
    assert_eq!(
        bg(text_style),
        Color::Indexed(236),
        "the band fills under the text"
    );
    if user_row > 0 {
        assert_ne!(
            bg(initial.style_at(user_row - 1, 8)),
            Color::Indexed(236),
            "the separator row above stays bare"
        );
    }

    let answer_row = initial.row_containing("answer body");
    let body_style = initial.first_symbol_style(answer_row);
    assert_eq!(
        fg(body_style),
        Color::Reset,
        "answers sit bare in the content column"
    );
    assert_eq!(bg(body_style), Color::Reset);

    let add_row = initial.row_containing("added");
    assert_eq!(
        bg(initial.first_symbol_style(add_row)),
        Color::Rgb(16, 40, 16),
        "diff add background"
    );
    let del_row = initial.row_containing("removed");
    assert_eq!(
        bg(initial.first_symbol_style(del_row)),
        Color::Rgb(40, 16, 16),
        "diff del background"
    );

    let collapsed = screen(&app);
    let header_row = collapsed.row_containing("think");
    let header_style = collapsed.first_symbol_style(header_row);
    assert_eq!(fg(header_style), Color::Rgb(130, 130, 155));
    assert!(header_style.add_modifier.contains(Modifier::BOLD));
    assert!(
        !collapsed
            .rows
            .iter()
            .any(|row| row.contains("quiet thought")),
        "sealed think blocks fold their body by default"
    );

    app.toggle_reasoning_block(6, 0);
    let expanded = screen(&app);
    let think_row = expanded.row_containing("think");
    let think_style = expanded.first_symbol_style(think_row);
    assert_eq!(fg(think_style), Color::Rgb(130, 130, 155));

    let body_row = expanded.row_containing("quiet thought");
    let body_style = expanded.first_symbol_style(body_row);
    assert_eq!(fg(body_style), Color::Rgb(130, 130, 155));
    assert!(body_style.add_modifier.contains(Modifier::ITALIC));

    let note_row = expanded.row_containing("a note");
    assert_eq!(fg(expanded.first_symbol_style(note_row)), Color::DarkGray);
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
    }]);
    let screen = screen(&app);

    // H1: accent orange, bold + underlined.
    let h1_row = screen.row_containing("Heading one");
    let h1 = screen.first_symbol_style(h1_row);
    assert!(
        h1.add_modifier.contains(Modifier::BOLD)
            && h1.add_modifier.contains(Modifier::UNDERLINED),
        "H1 keeps the top rung: {h1:?}"
    );
    assert_eq!(fg(h1), Color::Rgb(222, 137, 72), "H1 rides brand accent");

    // H2: bold accent, no underline.
    let h2_row = screen.row_containing("Heading two");
    let h2 = screen.first_symbol_style(h2_row);
    assert!(h2.add_modifier.contains(Modifier::BOLD));
    assert!(!h2.add_modifier.contains(Modifier::UNDERLINED));
    assert_eq!(fg(h2), Color::Rgb(222, 137, 72));

    // Task list: dim marker, accent checked box, struck+dim done body,
    // literal `[x]` glyphs.
    let task_row = screen.row_containing("done deal");
    let marker_column = char_column(&screen.rows[task_row], "-").expect("bullet");
    assert_eq!(
        fg(screen.style_at(task_row, marker_column)),
        Color::DarkGray,
        "the bullet is structural noise"
    );
    let glyph_column = char_column(&screen.rows[task_row], "[x]").expect("task glyph");
    assert_eq!(
        &screen.rows[task_row][glyph_column..glyph_column + 3],
        "[x]",
        "task glyphs stay literal (width safety)"
    );
    assert_eq!(
        fg(screen.style_at(task_row, glyph_column)),
        Color::Rgb(222, 137, 72),
        "the checked box lights up accent"
    );
    let body_column = char_column(&screen.rows[task_row], "done").expect("task body");
    let body_style = screen.style_at(task_row, body_column);
    assert!(body_style.add_modifier.contains(Modifier::CROSSED_OUT));
    assert_eq!(fg(body_style), Color::DarkGray);

    // Inline code: soft green text, no background block.
    let pill_row = screen.row_containing("fast_path");
    let pill_column = char_column(&screen.rows[pill_row], "fast_path").expect("code span");
    let code_style = screen.style_at(pill_row, pill_column);
    assert_eq!(
        fg(code_style),
        Color::Indexed(108),
        "code rides the stable indexed green without a palette"
    );
    assert_eq!(
        bg(code_style),
        Color::Reset,
        "code is font color only — no highlighter block"
    );

    // Links keep the blue convention with the underline.
    let link_row = screen.row_containing("docs");
    let link_column = char_column(&screen.rows[link_row], "docs").expect("link text");
    let link_style = screen.style_at(link_row, link_column);
    assert_eq!(fg(link_style), Color::Blue);
    assert!(link_style.add_modifier.contains(Modifier::UNDERLINED));
}

/// Display column of the first occurrence of `needle` in `row`.
fn char_column(row: &str, needle: &str) -> Option<usize> {
    let byte = row.find(needle)?;
    Some(row[..byte].chars().count())
}

#[test]
fn baseline_command_menu_pins_selection_and_usage_styles() {
    use crate::render::inset_band;

    let mut app = app();
    for ch in "/s".chars() {
        app.on_action(crate::app::action::Action::InsertChar(ch));
    }
    let menu = screen(&app);

    let highlighted = menu.row_containing("/sessions");
    // The float's side rail leads the row; the marker cell carries the
    // selection language.
    let marker_column = char_column(&menu.rows[highlighted], "›").expect("selected marker");
    let highlight_style = menu.style_at(highlighted, marker_column);
    assert_eq!(
        fg(highlight_style),
        Color::Rgb(222, 137, 72),
        "selected row text is accent orange"
    );
    assert_eq!(
        bg(highlight_style),
        Color::Rgb(64, 40, 22),
        "selection paints a full-row tinted background"
    );
    assert!(highlight_style.add_modifier.contains(Modifier::BOLD));

    // The rounded float anchors at the input band's left edge and spans
    // exactly the band's width; its top border carries both corner glyphs.
    let band = inset_band(ratatui::layout::Rect::new(0, 0, WIDTH, 1));
    let top_border = &menu.rows[highlighted - 1];
    let anchor_column = char_column(top_border, "╭").expect("rounded corner");
    let corner_column = char_column(top_border, "╮").expect("rounded corner");
    assert_eq!(anchor_column, usize::from(band.x));
    assert_eq!(corner_column - anchor_column + 1, usize::from(band.width));
    assert_eq!(
        bg(menu.style_at(highlighted, corner_column - 2)),
        Color::Rgb(64, 40, 22),
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
        Color::Reset,
        "unselected usage column is neutral; accent is reserved for activity"
    );
    assert_eq!(
        bg(menu.style_at(plain, corner_column - 2)),
        Color::Reset,
        "the menu floats on the native background (v0.44)"
    );
}
