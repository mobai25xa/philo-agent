//! Style-aware baseline assertions over TestBackend cells.
//!
//! Text snapshots cannot see colors; this file pins the current palette at
//! key coordinates so every token change here is deliberate and reviewed.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};

use crate::app::state::App;
use crate::app::status::StatusData;
use crate::app::transcript::{InfoLevel, LineKind, TranscriptLine};
use crate::render::frame::draw;
use crate::render::markdown::MarkdownRenderer;

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
    let markdown = MarkdownRenderer::new();
    terminal
        .draw(|frame| draw(frame, app, &markdown, false))
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
fn baseline_idle_screen_pins_band_rule_and_status_styles() {
    let app = app();
    let screen = screen(&app);

    assert!(
        !screen.rows.iter().any(|row| row.contains("───")),
        "idle separator rule stays deleted"
    );

    let composer_row = screen.row_containing("Ask anything");
    assert_eq!(
        bg(screen.style_at(composer_row, 0)),
        Color::Indexed(236),
        "composer band falls back to stable indexed gray without a palette"
    );
    assert_eq!(
        bg(screen.style_at(composer_row, usize::from(WIDTH) - 1)),
        Color::Indexed(236),
        "composer band reaches the right edge"
    );
    let gutter_column = screen.rows[composer_row]
        .find("Ask anything")
        .unwrap_or_else(|| panic!("placeholder text not found"));
    assert_eq!(
        fg(screen.first_symbol_style(composer_row)),
        Color::Rgb(222, 137, 72),
        "composer gutter keeps the accent"
    );
    assert_eq!(
        fg(screen.style_at(composer_row, gutter_column)),
        Color::DarkGray,
        "placeholder copy stays quiet"
    );

    let status_row = screen.row_containing("gpt-test");
    assert_eq!(
        fg(screen.first_symbol_style(status_row)),
        Color::DarkGray,
        "idle status is dim; orange is reserved for activity"
    );
}

#[test]
fn baseline_transcript_cells_pin_role_colors() {
    let mut app = app();
    app.cells.push_closed([
        TranscriptLine {
            kind: LineKind::User,
            text: String::new(),
        },
        TranscriptLine {
            kind: LineKind::User,
            text: "› hello there".to_owned(),
        },
        TranscriptLine {
            kind: LineKind::User,
            text: String::new(),
        },
        TranscriptLine {
            kind: LineKind::Answer,
            text: "answer body".to_owned(),
        },
        TranscriptLine {
            kind: LineKind::Tool,
            text: "+added".to_owned(),
        },
        TranscriptLine {
            kind: LineKind::Tool,
            text: "-removed".to_owned(),
        },
        TranscriptLine {
            kind: LineKind::Reasoning,
            text: "think".to_owned(),
        },
        TranscriptLine {
            kind: LineKind::Reasoning,
            text: "  quiet thought".to_owned(),
        },
        TranscriptLine {
            kind: LineKind::Meta,
            text: "a note".to_owned(),
        },
    ]);
    let initial = screen(&app);

    let user_row = initial.row_containing("hello there");
    let gutter_style = initial.first_symbol_style(user_row);
    assert_eq!(fg(gutter_style), Color::Rgb(222, 137, 72));
    assert!(gutter_style.add_modifier.contains(Modifier::BOLD));
    let tail_column = initial.rows[user_row].len() + 1;
    assert_eq!(
        bg(initial.style_at(user_row, tail_column)),
        Color::Indexed(236),
        "user band fills the content column"
    );

    let answer_row = initial.row_containing("answer body");
    let bullet_style = initial.first_symbol_style(answer_row);
    assert_eq!(fg(bullet_style), Color::Rgb(222, 137, 72));
    assert_eq!(bg(bullet_style), Color::Reset);

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
    let header_row = collapsed.row_containing("think · 1 行");
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
fn baseline_command_menu_pins_selection_and_usage_styles() {
    let mut app = app();
    for ch in "/s".chars() {
        app.on_action(crate::app::action::Action::InsertChar(ch));
    }
    let menu = screen(&app);

    let highlighted = menu.row_containing("/sessions");
    let highlight_style = menu.first_symbol_style(highlighted);
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
    let last_column = usize::from(WIDTH) - 1;
    assert_eq!(
        bg(menu.style_at(highlighted, last_column)),
        Color::Rgb(64, 40, 22),
        "the fill runs to the right edge"
    );

    let plain = menu
        .rows
        .iter()
        .enumerate()
        .position(|(_, row)| row.contains("/status"))
        .expect("unselected candidate visible");
    assert_eq!(
        fg(menu.first_symbol_style(plain)),
        Color::Reset,
        "unselected usage column is neutral; accent is reserved for activity"
    );
    assert_eq!(
        bg(menu.style_at(plain, last_column)),
        Color::Indexed(236),
        "the menu floats on its own panel wash"
    );
}
