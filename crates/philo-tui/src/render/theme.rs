//! Internal presentation colors. This is not a user-facing theme system.

use ratatui::style::{Color, Modifier, Style};

pub(crate) fn user() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn reasoning() -> Style {
    Style::default()
        .fg(Color::Rgb(130, 130, 155))
        .add_modifier(Modifier::ITALIC)
}

pub(crate) fn answer() -> Style {
    Style::default()
}

pub(crate) fn tool() -> Style {
    Style::default().fg(Color::Cyan)
}

pub(crate) fn tool_ok() -> Style {
    Style::default().fg(Color::Green)
}

pub(crate) fn tool_err() -> Style {
    Style::default().fg(Color::Red)
}

pub(crate) fn notice() -> Style {
    Style::default().fg(Color::Yellow)
}

pub(crate) fn error() -> Style {
    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
}

pub(crate) fn meta() -> Style {
    Style::default().fg(Color::DarkGray)
}

pub(crate) fn border() -> Style {
    Style::default().fg(Color::DarkGray)
}

pub(crate) fn border_focus() -> Style {
    Style::default().fg(Color::Cyan)
}

pub(crate) fn border_warning() -> Style {
    Style::default().fg(Color::Yellow)
}

pub(crate) fn placeholder() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC)
}

pub(crate) fn gutter() -> Style {
    Style::default().fg(Color::Cyan)
}

pub(crate) fn rule() -> Style {
    Style::default().fg(Color::DarkGray)
}

pub(crate) fn activity_normal() -> Style {
    Style::default().fg(Color::Cyan)
}

pub(crate) fn activity_reasoning() -> Style {
    Style::default().fg(Color::Rgb(130, 130, 155))
}

pub(crate) fn activity_tool() -> Style {
    Style::default().fg(Color::Blue)
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
