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
) -> Vec<Line<'static>> {
    slice
        .rows
        .iter()
        .map(|row| {
            let line = TranscriptLine {
                kind: row.kind,
                text: row.text.clone(),
            };
            let painted = match row.kind {
                LineKind::Answer => markdown.preview(&line),
                _ => crate::render::line::styled_line(&line),
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
