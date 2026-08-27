//! Style and paint a display `VisibleSlice`. No scroll policy.
//!
//! User strip rows arrive as bare primary text: the full-width surface band
//! and column-0 accent bar are pre-painted by the frame under them (design
//! §3.1), so this layer never fills their background. Answer rows paint
//! from their projected block role (`app::prose`) — pure, per row.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::app::cells::{VisibleRow, VisibleSlice};
use crate::app::select::Selection;
use crate::app::text;
use crate::app::transcript::{LineKind, Tone};

use super::markdown;
use super::theme;

/// Project each visible row with role-driven styling. Pure: no renderer
/// state exists anywhere on this path.
pub(crate) fn paint_slice(
    slice: &VisibleSlice,
    selection: Option<Selection>,
    width: usize,
) -> Vec<Line<'static>> {
    slice
        .rows
        .iter()
        .map(|row| {
            let painted = match row.kind {
                LineKind::Answer => paint_answer(row),
                _ => styled_row(row),
            };
            let painted = match row.tone {
                Tone::DiffDel | Tone::DiffIns => fill_diff_line(painted, width),
                _ => painted,
            };
            let painted = highlight_row(
                painted,
                selection,
                row.cell_index,
                row.row_in_cell,
                text::width(&row.text),
            );
            // User strips ride the band's bar-gap rhythm: one space of air
            // after the pre-painted bar (v2.3). Applied after selection so
            // the highlight offsets stay text-relative.
            match row.kind {
                LineKind::User if !row.text.is_empty() => prefixed_line(painted),
                _ => painted,
            }
        })
        .collect()
}

/// Prepends one blank cell to a painted line (paint-time only; the stored
/// wrap rows and copied text stay untouched).
fn prefixed_line(line: Line<'static>) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::raw(" "));
    spans.extend(line.spans);
    Line::from(spans)
}

fn styled_row(row: &VisibleRow) -> Line<'static> {
    crate::render::line::styled_line(&crate::app::transcript::TranscriptLine {
        kind: row.kind,
        text: row.text.clone(),
        tone: row.tone,
    })
}

/// Answer rows go straight into the content column — no gutter — and paint
/// from their baked spans (or syntect, for fenced bodies): design §3.2,
/// plan P0/P2.
fn paint_answer(row: &VisibleRow) -> Line<'static> {
    markdown::answer_row(&row.text, &row.role, row.spans.as_deref())
}

fn fill_diff_line(line: Line<'static>, width: usize) -> Line<'static> {
    let style = line.style;
    if style.bg.is_none() {
        return line;
    }
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
    use crate::app::prose::BlockRole;
    use crate::app::transcript::LineKind;
    use ratatui::style::Modifier;

    use super::*;

    fn paint_tool(text: &str, tone: Tone, width: usize) -> Line<'static> {
        let slice = VisibleSlice {
            rows: vec![VisibleRow {
                cell_index: 0,
                row_in_cell: 0,
                kind: LineKind::Tool,
                tone,
                role: BlockRole::Plain,
                spans: None,
                text: text.to_owned(),
            }],
            total_rows: 1,
            follow_bottom: true,
            at_top: true,
            at_bottom: true,
        };
        let mut lines = paint_slice(&slice, None, width);
        assert_eq!(lines.len(), 1);
        lines.remove(0)
    }

    /// A selection overlay splits styled spans without losing their base
    /// styles (plan TP2.4).
    #[test]
    fn selection_cuts_through_styled_answer_spans() {
        use crate::app::prose;
        use crate::app::select::SelectPos;

        let rows = prose::project_answer("**bold words** tail", 80);
        let row = &rows[0];
        assert!(row.spans.as_ref().expect("styled").len() > 1);

        let mut selection = Selection::start(SelectPos {
            cell: 0,
            row: 0,
            col: 0,
        });
        selection.head = SelectPos {
            cell: 0,
            row: 0,
            col: 6,
        };
        selection.dragging = false;

        let slice = VisibleSlice {
            rows: vec![VisibleRow {
                cell_index: 0,
                row_in_cell: 0,
                kind: LineKind::Answer,
                tone: Tone::Plain,
                role: row.role.clone(),
                spans: row.spans.clone(),
                text: row.text.clone(),
            }],
            total_rows: 1,
            follow_bottom: true,
            at_top: true,
            at_bottom: true,
        };
        let painted = paint_slice(&slice, Some(selection), 80).remove(0);
        let bolds: String = painted
            .spans
            .iter()
            .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(bolds, "bold words", "the bold run survives span splitting");
        let reversed: String = painted
            .spans
            .iter()
            .filter(|span| span.style.add_modifier.contains(Modifier::REVERSED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(reversed, "bold w", "exactly the selected range highlights");
        let rest: String = painted
            .spans
            .iter()
            .filter(|span| !span.style.add_modifier.contains(Modifier::REVERSED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(rest, "ords tail");
    }

    #[test]
    fn tool_diff_rows_wash_full_content_column() {
        const WIDTH: usize = 20;
        let add = paint_tool("    2 | world", Tone::DiffIns, WIDTH);
        assert_eq!(add.width(), WIDTH);
        assert_eq!(add.style, theme::diff_add());

        let del = paint_tool("    1 | hello", Tone::DiffDel, WIDTH);
        assert_eq!(del.width(), WIDTH);
        assert_eq!(del.style, theme::diff_del());

        let header = paint_tool("Edit src/lib.rs", Tone::Title, 40);
        assert_eq!(
            header.spans[0].style,
            theme::accent().add_modifier(ratatui::style::Modifier::BOLD)
        );
        assert_ne!(header.style, theme::diff_add());
        assert_ne!(header.style, theme::diff_del());

        let context = paint_tool("    3 | tail", Tone::Plain, WIDTH);
        assert_eq!(context.width(), text::width("    3 | tail"));
        assert_ne!(context.style, theme::diff_add());
        assert_ne!(context.style, theme::diff_del());
    }
}
