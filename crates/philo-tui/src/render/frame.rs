//! Pure projection of app state into one Ratatui frame.

use ratatui::layout::{Constraint, Layout};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::app::state::App;
use crate::app::transcript::TranscriptLine;

use super::line::styled_line;

const INPUT_WINDOW: usize = 5;
const OVERLAY_BODY: usize = 5;

pub(crate) fn draw(frame: &mut ratatui::Frame<'_>, app: &App, shift_enter: bool) {
    use ratatui::style::{Color, Modifier, Style};

    let live = app.transcript.partial().map(|(kind, text)| TranscriptLine {
        kind,
        text: text.to_owned(),
    });
    let input_lines = app.input.lines().to_vec();
    let (cursor_row, cursor_col) = app.input.cursor();
    let status_text = app.status.line();
    let overlay = app.overlay_frame(OVERLAY_BODY);
    let completion = app.completion_line();

    if let Some(overlay) = overlay {
        let [panel_area, status_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());

        let mut lines = vec![Line::styled(
            overlay.title,
            Style::default().add_modifier(Modifier::BOLD),
        )];
        lines.extend(overlay.body.into_iter().map(Line::from));
        lines.push(Line::styled(
            overlay.footer,
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(Paragraph::new(lines), panel_area);
        frame.render_widget(
            Paragraph::new(Line::styled(
                status_text,
                Style::default().add_modifier(Modifier::REVERSED),
            )),
            status_area,
        );
        return;
    }

    let [live_area, input_area, hint_area, status_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(INPUT_WINDOW as u16),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    if let Some(live) = &live {
        frame.render_widget(Paragraph::new(styled_line(live)), live_area);
    }

    let first_visible = cursor_row.saturating_sub(INPUT_WINDOW - 1);
    let visible: Vec<Line<'_>> = input_lines
        .iter()
        .enumerate()
        .skip(first_visible)
        .take(INPUT_WINDOW)
        .map(|(index, text)| {
            let prefix = if index == 0 { "> " } else { "| " };
            Line::from(format!("{prefix}{text}"))
        })
        .collect();
    frame.render_widget(Paragraph::new(visible), input_area);

    let hint = completion.unwrap_or_else(|| {
        let newline_hint = if shift_enter {
            "Shift+Enter/Ctrl+J newline"
        } else {
            "Ctrl+J newline"
        };
        format!("Enter send | {newline_hint} | Esc cancel | Ctrl+C clear/exit | Ctrl+O detail")
    });
    frame.render_widget(
        Paragraph::new(Line::styled(hint, Style::default().fg(Color::DarkGray))),
        hint_area,
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            status_text,
            Style::default().add_modifier(Modifier::REVERSED),
        )),
        status_area,
    );

    let cursor_x = input_area.x
        + 2
        + u16::try_from(
            app.input.lines()[cursor_row]
                .chars()
                .take(cursor_col)
                .count(),
        )
        .unwrap_or(0);
    let cursor_y = input_area.y + u16::try_from(cursor_row - first_visible).unwrap_or(0);
    frame.set_cursor_position((cursor_x, cursor_y));
}
