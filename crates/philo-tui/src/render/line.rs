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
    if text.is_empty() {
        return Line::from("").style(theme::user_band());
    }
    if let Some(rest) = text.strip_prefix("› ") {
        return Line::from(vec![
            Span::styled("› ".to_owned(), theme::user_gutter()),
            Span::styled(rest.to_owned(), theme::user()),
        ]);
    }
    if let Some(rest) = text.strip_prefix("  ") {
        return Line::from(vec![
            Span::styled("  ".to_owned(), theme::user_band()),
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
    let rest = text
        .strip_prefix("│ ")
        .or_else(|| text.strip_prefix("  "))
        .unwrap_or(text);
    Line::from(vec![
        Span::styled("│ ".to_owned(), theme::reasoning()),
        Span::styled(rest.to_owned(), theme::reasoning()),
    ])
}

fn tool_line(text: &str) -> Line<'static> {
    if let Some(rest) = text.strip_prefix("• ") {
        return default_tool_header(rest);
    }
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
    if let Some(rest) = text.strip_prefix('-') {
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        return Line::from(vec![
            Span::styled("- ".to_owned(), theme::diff_del()),
            Span::styled(rest.to_owned(), theme::diff_del()),
        ]);
    }
    if let Some(rest) = text.strip_prefix('+') {
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        return Line::from(vec![
            Span::styled("+ ".to_owned(), theme::diff_add()),
            Span::styled(rest.to_owned(), theme::diff_add()),
        ]);
    }
    if let Some(rest) = text.strip_prefix("  ") {
        return Line::from(vec![
            Span::styled("  ".to_owned(), theme::meta()),
            Span::styled(rest.to_owned(), theme::meta()),
        ]);
    }
    Line::styled(text.to_owned(), theme::tool())
}

fn default_tool_header(rest: &str) -> Line<'static> {
    let (verb, detail) = rest
        .split_once("  ")
        .map(|(verb, detail)| (verb, Some(detail)))
        .unwrap_or((rest, None));
    let mut spans = vec![
        Span::styled("• ".to_owned(), theme::tool()),
        Span::styled(verb.to_owned(), theme::tool().add_modifier(Modifier::BOLD)),
    ];
    if let Some(detail) = detail {
        spans.extend(header_rest_spans(detail));
    }
    Line::from(spans)
}

fn header_rest_spans(rest: &str) -> Vec<Span<'static>> {
    if rest.contains("error") {
        return vec![Span::styled(format!("  {rest}"), theme::tool_err())];
    }
    let mut spans = vec![Span::styled("  ".to_owned(), theme::meta())];
    if let Some(open) = rest.find("(+")
        && let Some(rel_close) = rest[open..].find(')')
    {
        let close = open + rel_close;
        let before = &rest[..open];
        let inner = &rest[open + 1..close];
        let after = &rest[close + 1..];
        if !before.is_empty() {
            spans.push(Span::styled(before.to_owned(), theme::meta()));
        }
        spans.push(Span::styled("(".to_owned(), theme::meta()));
        if let Some((added, removed)) = inner.split_once(' ') {
            spans.push(Span::styled(added.to_owned(), theme::diff_add()));
            spans.push(Span::styled(" ".to_owned(), theme::meta()));
            spans.push(Span::styled(removed.to_owned(), theme::diff_del()));
        } else {
            spans.push(Span::styled(inner.to_owned(), theme::meta()));
        }
        spans.push(Span::styled(")".to_owned(), theme::meta()));
        if !after.is_empty() {
            spans.push(Span::styled(after.to_owned(), theme::meta()));
        }
        return spans;
    }
    spans.push(Span::styled(rest.to_owned(), theme::meta()));
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn think_block_paints_bar_on_body_not_header() {
        let header = styled_line(&TranscriptLine {
            kind: LineKind::Reasoning,
            text: "think".to_owned(),
        });
        assert_eq!(line_text(&header), "think");
        assert!(!line_text(&header).contains('│'));

        let body = styled_line(&TranscriptLine {
            kind: LineKind::Reasoning,
            text: "  hello".to_owned(),
        });
        assert_eq!(line_text(&body), "│ hello");

        let wrapped = styled_line(&TranscriptLine {
            kind: LineKind::Reasoning,
            text: "│ more".to_owned(),
        });
        assert_eq!(line_text(&wrapped), "│ more");

        let answer = styled_line(&TranscriptLine {
            kind: LineKind::Answer,
            text: "• the actual answer".to_owned(),
        });
        assert!(line_text(&answer).starts_with("• "));
        assert!(!line_text(&answer).contains('│'));
    }

    #[test]
    fn tool_diff_paints_a_two_cell_gutter_without_doubling() {
        assert_eq!(line_text(&tool_line("+bar")), "+ bar");
        assert_eq!(line_text(&tool_line("+ bar")), "+ bar");
        assert_eq!(line_text(&tool_line("-old")), "- old");
        assert_eq!(line_text(&tool_line("- old")), "- old");
    }
}
