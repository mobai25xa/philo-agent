//! Semantic transcript lines mapped to Ratatui styles.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::app::transcript::{LineKind, TranscriptLine};

use super::theme;

pub(crate) fn styled_line(line: &TranscriptLine) -> Line<'static> {
    match line.kind {
        LineKind::User => user_line(&line.text),
        LineKind::Reasoning => reasoning_line(&line.text),
        LineKind::Tool => tool_line(&line.text),
        LineKind::Answer => Line::styled(line.text.clone(), theme::answer()),
        LineKind::Notice => Line::styled(line.text.clone(), theme::notice()),
        LineKind::Error => Line::styled(line.text.clone(), theme::error()),
        LineKind::Meta => Line::styled(line.text.clone(), theme::meta()),
    }
}

fn user_line(text: &str) -> Line<'static> {
    if text == "You" {
        return Line::styled(text.to_owned(), theme::user());
    }
    if let Some(rest) = text.strip_prefix("  ") {
        return Line::from(vec![
            Span::styled("  ", theme::gutter()),
            Span::styled(rest.to_owned(), theme::user()),
        ]);
    }
    if let Some(rest) = text.strip_prefix("> ") {
        return Line::from(vec![
            Span::styled("> ", theme::gutter()),
            Span::styled(rest.to_owned(), theme::user()),
        ]);
    }
    Line::styled(text.to_owned(), theme::user())
}

fn reasoning_line(text: &str) -> Line<'static> {
    if text == "think" || text.starts_with("think · ") {
        return Line::styled(
            text.to_owned(),
            theme::reasoning().add_modifier(Modifier::BOLD),
        );
    }
    Line::styled(text.to_owned(), theme::reasoning())
}

fn tool_line(text: &str) -> Line<'static> {
    if let Some(rest) = text.strip_prefix("▸ ") {
        let (name, detail) = rest
            .split_once("  ")
            .map(|(name, detail)| (name, Some(detail)))
            .unwrap_or((rest, None));
        let mut spans = vec![
            Span::styled("▸ ", theme::tool()),
            Span::styled(name.to_owned(), theme::tool().add_modifier(Modifier::BOLD)),
        ];
        if let Some(detail) = detail {
            let style = if detail.contains("error") {
                theme::tool_err()
            } else if detail.starts_with("ok")
                || detail.contains("lines")
                || detail.contains("matches")
                || detail.contains("entries")
                || detail.contains("exit 0")
                || detail.contains("replaced")
                || detail.contains("created")
                || detail.contains("overwrote")
                || detail.contains("wrote")
            {
                theme::tool_ok()
            } else {
                theme::meta()
            };
            spans.push(Span::styled(format!("  {detail}"), style));
        }
        return Line::from(spans);
    }
    if let Some(rest) = text.strip_prefix("  └ ") {
        let style = if rest.contains("error") || rest.starts_with("exit ") {
            theme::tool_err()
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
