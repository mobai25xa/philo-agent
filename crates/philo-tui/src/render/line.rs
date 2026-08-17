//! Semantic transcript lines mapped to Ratatui styles.

use ratatui::text::{Line, Span};

use crate::app::transcript::{LineKind, TranscriptLine};

use super::theme;

pub(crate) fn styled_line(line: &TranscriptLine) -> Line<'static> {
    match line.kind {
        LineKind::User => user_line(&line.text),
        LineKind::Reasoning => Line::styled(line.text.clone(), theme::reasoning()),
        LineKind::Tool => tool_line(&line.text),
        LineKind::Answer => Line::styled(line.text.clone(), theme::answer()),
        LineKind::Notice => Line::styled(line.text.clone(), theme::notice()),
        LineKind::Error => Line::styled(line.text.clone(), theme::error()),
        LineKind::Meta => Line::styled(line.text.clone(), theme::meta()),
    }
}

fn user_line(text: &str) -> Line<'static> {
    if let Some(rest) = text.strip_prefix("> ") {
        return Line::from(vec![
            Span::styled("> ", theme::gutter()),
            Span::styled(rest.to_owned(), theme::user()),
        ]);
    }
    Line::styled(text.to_owned(), theme::user())
}

fn tool_line(text: &str) -> Line<'static> {
    if let Some(rest) = text.strip_prefix("▸ ") {
        let (name, detail) = rest
            .split_once("  ")
            .map(|(name, detail)| (name, Some(detail)))
            .unwrap_or((rest, None));
        let mut spans = vec![
            Span::styled("▸ ", theme::tool()),
            Span::styled(
                name.to_owned(),
                theme::tool().add_modifier(ratatui::style::Modifier::BOLD),
            ),
        ];
        if let Some(detail) = detail {
            let style = if detail.starts_with("error") {
                theme::tool_err()
            } else if detail.starts_with("ok") {
                theme::tool_ok()
            } else {
                theme::meta()
            };
            spans.push(Span::styled(format!("  {detail}"), style));
        }
        return Line::from(spans);
    }
    if let Some(rest) = text.strip_prefix("  └ ") {
        let style = if rest.starts_with("error") {
            theme::tool_err()
        } else if rest.starts_with("ok") {
            theme::tool_ok()
        } else {
            theme::meta()
        };
        return Line::from(vec![
            Span::styled("  └ ", theme::meta()),
            Span::styled(rest.to_owned(), style),
        ]);
    }
    Line::styled(text.to_owned(), theme::tool())
}
