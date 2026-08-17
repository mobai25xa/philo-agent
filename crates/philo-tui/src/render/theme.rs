//! Internal presentation colors. This is not a user-facing theme system.

use ratatui::style::{Color, Modifier, Style};

const USER_BAND: Color = Color::Rgb(38, 40, 48);
const ACCENT: Color = Color::Green;

pub(crate) fn user_band() -> Style {
    Style::default().bg(USER_BAND)
}

pub(crate) fn user() -> Style {
    user_band()
}

pub(crate) fn user_gutter() -> Style {
    user_band().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub(crate) fn reasoning() -> Style {
    Style::default()
        .fg(Color::Rgb(130, 130, 155))
        .add_modifier(Modifier::ITALIC)
}

pub(crate) fn answer() -> Style {
    Style::default()
}

pub(crate) fn answer_gutter() -> Style {
    Style::default().fg(ACCENT)
}

pub(crate) fn tool() -> Style {
    Style::default().fg(ACCENT)
}

pub(crate) fn tool_ok() -> Style {
    Style::default().fg(Color::Green)
}

pub(crate) fn tool_err() -> Style {
    Style::default().fg(Color::Red)
}

pub(crate) fn diff_add() -> Style {
    Style::default().fg(Color::Green).bg(Color::Rgb(16, 40, 16))
}

pub(crate) fn diff_del() -> Style {
    Style::default().fg(Color::Red).bg(Color::Rgb(40, 16, 16))
}

pub(crate) fn notice() -> Style {
    Style::default().fg(Color::Yellow)
}

pub(crate) fn error() -> Style {
    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
}

pub(crate) fn selection() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

pub(crate) fn meta() -> Style {
    Style::default().fg(Color::DarkGray)
}

pub(crate) fn placeholder() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC)
}

pub(crate) fn rule() -> Style {
    Style::default().fg(Color::DarkGray)
}

pub(crate) fn activity_normal() -> Style {
    Style::default().fg(ACCENT)
}

pub(crate) fn activity_reasoning() -> Style {
    Style::default().fg(Color::Rgb(130, 130, 155))
}

pub(crate) fn activity_tool() -> Style {
    Style::default().fg(ACCENT)
}

pub(crate) fn activity_warning() -> Style {
    Style::default().fg(Color::Yellow)
}

pub(crate) fn status_idle() -> Style {
    Style::default().fg(Color::Green)
}

pub(crate) fn status_busy() -> Style {
    Style::default().fg(Color::Yellow)
}
