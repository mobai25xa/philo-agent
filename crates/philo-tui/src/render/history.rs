//! Style and paint a display `VisibleSlice`. No scroll policy.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::app::cells::VisibleSlice;
use crate::app::select::Selection;
use crate::app::text;
use crate::app::transcript::{LineKind, TranscriptLine};

use super::markdown::MarkdownRenderer;
use super::theme;

/// Project each visible row with preview styling. History never calls
/// `markdown.commit`.
pub(crate) fn paint_slice(
    markdown: &MarkdownRenderer,
    slice: &VisibleSlice,
    selection: Option<Selection>,
    width: usize,
) -> Vec<Line<'static>> {
    slice
        .rows
        .iter()
        .map(|row| {
            let painted = match row.kind {
                LineKind::Answer => paint_answer(markdown, &row.text),
                _ => crate::render::line::styled_line(&TranscriptLine {
                    kind: row.kind,
                    text: row.text.clone(),
                }),
            };
            let painted = match row.kind {
                LineKind::User => fill_user_line(painted, width),
                LineKind::Tool => fill_diff_line(painted, &row.text, width),
                _ => painted,
            };
            highlight_row(
                painted,
                selection,
                row.cell_index,
                row.row_in_cell,
                text::width(&row.text),
            )
        })
        .collect()
}

fn paint_answer(markdown: &MarkdownRenderer, text: &str) -> Line<'static> {
    let (gutter, content) = split_answer_gutter(text);
    let painted = markdown.preview(&TranscriptLine {
        kind: LineKind::Answer,
        text: content.to_owned(),
    });
    if gutter.is_empty() {
        return painted;
    }
    let mut spans = vec![Span::styled(gutter.to_owned(), theme::answer_gutter())];
    spans.extend(painted.spans);
    Line::from(spans)
}

fn split_answer_gutter(text: &str) -> (&str, &str) {
    if let Some(rest) = text.strip_prefix("• ") {
        ("• ", rest)
    } else if let Some(rest) = text.strip_prefix("  ") {
        ("  ", rest)
    } else {
        ("", text)
    }
}

fn fill_user_line(line: Line<'static>, width: usize) -> Line<'static> {
    let used = line.width();
    let mut spans = line.spans;
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), theme::user_band()));
    }
    Line::from(spans).style(theme::user_band())
}

fn fill_diff_line(line: Line<'static>, text: &str, width: usize) -> Line<'static> {
    let style = if text.starts_with('+') {
        theme::diff_add()
    } else if text.starts_with('-') {
        theme::diff_del()
    } else {
        return line;
    };
    let used = line.width();
    let mut spans = line.spans;
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), style));
    }
    Line::from(spans).style(style)
}

pub(crate) fn highlight_row(
    line: Line<'static>,
    selection: Option<Selection>,
    cell: usize,
    row: usize,
    row_width: usize,
) -> Line<'static> {
    let Some(selection) = selection else {
        return line;
    };
    let Some((from, to)) = selection.columns_on_row(cell, row, row_width) else {
        return line;
    };
    apply_column_range(line, from, to, theme::selection())
}

/// Overlay `style` onto the `[from, to)` display-column range of `line`.
pub(crate) fn apply_column_range(
    line: Line<'static>,
    from: usize,
    to: usize,
    style: Style,
) -> Line<'static> {
    if from >= to {
        return line;
    }
    let mut col = 0;
    let mut spans = Vec::new();
    for span in line.spans {
        let span_width = text::width(&span.content);
        let span_end = col + span_width;
        if span_end <= from || col >= to {
            spans.push(span);
            col = span_end;
            continue;
        }
        let local_from = from.saturating_sub(col);
        let local_to = (to - col).min(span_width);
        split_span(&span, local_from, local_to, style, &mut spans);
        col = span_end;
    }
    Line::from(spans)
}

fn split_span(span: &Span<'_>, from: usize, to: usize, style: Style, out: &mut Vec<Span<'static>>) {
    let content = span.content.as_ref();
    let before = text::slice_columns(content, 0, from);
    let mid = text::slice_columns(content, from, to);
    let after = text::slice_columns(content, to, text::width(content));
    if !before.is_empty() {
        out.push(Span::styled(before, span.style));
    }
    if !mid.is_empty() {
        out.push(Span::styled(mid, span.style.patch(style)));
    }
    if !after.is_empty() {
        out.push(Span::styled(after, span.style));
    }
}

#[cfg(test)]
mod tests {
    use crate::app::cells::{VisibleRow, VisibleSlice};
    use crate::app::transcript::LineKind;
    use crate::render::markdown::MarkdownRenderer;

    use super::*;

    fn paint_tool(text: &str, width: usize) -> Line<'static> {
        let markdown = MarkdownRenderer::new();
        let slice = VisibleSlice {
            rows: vec![VisibleRow {
                cell_index: 0,
                row_in_cell: 0,
                kind: LineKind::Tool,
                text: text.to_owned(),
            }],
            total_rows: 1,
            follow_bottom: true,
            at_top: true,
            at_bottom: true,
        };
        let mut lines = paint_slice(&markdown, &slice, None, width);
        assert_eq!(lines.len(), 1);
        lines.remove(0)
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn tool_diff_rows_wash_full_content_column() {
        const WIDTH: usize = 20;
        let add = paint_tool("+bar", WIDTH);
        assert_eq!(add.width(), WIDTH);
        assert_eq!(add.style, theme::diff_add());
        assert!(line_text(&add).starts_with("+ bar"));
        assert!(!line_text(&add).starts_with("+  bar"));

        let already = paint_tool("+ bar", WIDTH);
        assert_eq!(already.width(), WIDTH);
        assert!(line_text(&already).starts_with("+ bar"));
        assert!(!line_text(&already).starts_with("+  bar"));

        let del = paint_tool("-foo", WIDTH);
        assert_eq!(del.width(), WIDTH);
        assert_eq!(del.style, theme::diff_del());
        assert!(line_text(&del).starts_with("- foo"));

        let header = paint_tool("• Edited  src/lib.rs  (+1 -1)", 40);
        assert_eq!(header.width(), text::width("• Edited  src/lib.rs  (+1 -1)"));
        assert_ne!(header.style, theme::diff_add());
        assert_ne!(header.style, theme::diff_del());

        let context = paint_tool("  foo", WIDTH);
        assert_eq!(context.width(), text::width("  foo"));
        assert_ne!(context.style, theme::diff_add());
        assert_ne!(context.style, theme::diff_del());
    }
}
