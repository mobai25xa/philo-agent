//! Semantic transcript lines mapped to Ratatui styles.

use ratatui::text::Line;

use crate::app::transcript::{LineKind, TranscriptLine};

pub(crate) fn styled_line(line: &TranscriptLine) -> Line<'static> {
    use ratatui::style::{Color, Modifier, Style};

    let style = match line.kind {
        LineKind::Answer => Style::default(),
        LineKind::User => Style::default().add_modifier(Modifier::BOLD),
        LineKind::Reasoning => Style::default().fg(Color::DarkGray),
        LineKind::Tool => Style::default().fg(Color::Cyan),
        LineKind::Notice => Style::default().fg(Color::Yellow),
        LineKind::Error => Style::default().fg(Color::Red),
        LineKind::Meta => Style::default().fg(Color::DarkGray),
    };
    Line::styled(line.text.clone(), style)
}
