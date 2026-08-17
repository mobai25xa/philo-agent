//! Soft-wrapped, cursor-following projection of the input editor.

use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use crate::app::input::InputEditor;
use crate::app::text;

use super::theme;

const GUTTER_WIDTH: usize = 2;
const FIRST_GUTTER: &str = "› ";
const WRAP_GUTTER: &str = "  ";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ComposerViewport {
    pub(crate) rows: Vec<String>,
    pub(crate) first_visual_row: usize,
    pub(crate) cursor_x: usize,
    pub(crate) cursor_y: usize,
}

pub(crate) fn viewport(input: &InputEditor, width: usize, height: usize) -> ComposerViewport {
    if width == 0 || height == 0 {
        return ComposerViewport {
            rows: Vec::new(),
            first_visual_row: 0,
            cursor_x: 0,
            cursor_y: 0,
        };
    }

    let gutter_width = GUTTER_WIDTH.min(width.saturating_sub(1));
    let content_width = width.saturating_sub(gutter_width).max(1);
    let (cursor_line, cursor_byte) = input.cursor_byte();
    let mut visual_rows = Vec::new();
    let mut cursor_visual = (0, 0);

    for (logical_row, line) in input.lines().iter().enumerate() {
        let line_cursor = (logical_row == cursor_line).then_some(cursor_byte);
        let (wrapped, cursor) = wrap_line(line, line_cursor, content_width);
        let visual_start = visual_rows.len();
        for (visual_index, content) in wrapped.into_iter().enumerate() {
            let gutter = match (logical_row, visual_index) {
                (0, 0) => FIRST_GUTTER,
                _ => WRAP_GUTTER,
            };
            let gutter = text::truncate(gutter, gutter_width);
            visual_rows.push(format!("{gutter}{content}"));
        }
        if let Some((row, cell)) = cursor {
            cursor_visual = (visual_start + row, gutter_width + cell);
        }
    }

    let first_visual_row = cursor_visual.0.saturating_sub(height - 1);
    let rows = visual_rows
        .into_iter()
        .skip(first_visual_row)
        .take(height)
        .collect();
    ComposerViewport {
        rows,
        first_visual_row,
        cursor_x: cursor_visual.1.min(width.saturating_sub(1)),
        cursor_y: cursor_visual.0 - first_visual_row,
    }
}

pub(crate) fn styled_rows(rows: &[String], empty_placeholder: Option<&str>) -> Vec<Line<'static>> {
    if rows.is_empty() {
        return Vec::new();
    }
    if let Some(placeholder) = empty_placeholder {
        return vec![Line::from(vec![
            Span::styled(FIRST_GUTTER.to_owned(), theme::gutter()),
            Span::styled(placeholder.to_owned(), theme::placeholder()),
        ])];
    }
    rows.iter()
        .map(|row| {
            let (gutter, content) = split_gutter(row);
            Line::from(vec![
                Span::styled(gutter.to_owned(), theme::gutter()),
                Span::raw(content.to_owned()),
            ])
        })
        .collect()
}

fn split_gutter(row: &str) -> (&str, &str) {
    let mut cells = 0;
    let mut end = 0;
    for (index, grapheme) in row.grapheme_indices(true) {
        let grapheme_width = text::width(grapheme);
        if cells + grapheme_width > GUTTER_WIDTH {
            break;
        }
        cells += grapheme_width;
        end = index + grapheme.len();
        if cells >= GUTTER_WIDTH {
            break;
        }
    }
    row.split_at(end)
}

fn wrap_line(
    line: &str,
    cursor_byte: Option<usize>,
    max_width: usize,
) -> (Vec<String>, Option<(usize, usize)>) {
    let mut rows = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    let mut cursor = None;

    for (byte, grapheme) in line.grapheme_indices(true) {
        let grapheme_width = text::width(grapheme);
        if current_width > 0 && current_width + grapheme_width > max_width {
            rows.push(std::mem::take(&mut current));
            current_width = 0;
        }
        if cursor_byte == Some(byte) {
            cursor = Some((rows.len(), current_width));
        }
        current.push_str(grapheme);
        current_width += grapheme_width;
    }

    if cursor_byte == Some(line.len()) {
        if current_width >= max_width && !current.is_empty() {
            rows.push(std::mem::take(&mut current));
            current_width = 0;
        }
        cursor = Some((rows.len(), current_width));
    }
    rows.push(current);
    (rows, cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_and_emoji_wrap_by_cells_without_splitting_graphemes() {
        let mut input = InputEditor::new();
        input.insert_str("中文👩‍💻ab");
        let view = viewport(&input, 6, 4);
        assert_eq!(view.rows, ["› 中文", "  👩‍💻ab", "  "]);
        assert_eq!((view.cursor_x, view.cursor_y), (2, 2));
    }

    #[test]
    fn combining_character_occupies_one_cell() {
        let mut input = InputEditor::new();
        input.insert_str("e\u{301}x");
        let view = viewport(&input, 8, 2);
        assert_eq!(view.rows, ["› e\u{301}x"]);
        assert_eq!(view.cursor_x, 4);
    }

    #[test]
    fn long_input_scrolls_inside_the_fixed_window() {
        let mut input = InputEditor::new();
        input.insert_str("one\ntwo\nthree\nfour\nfive");
        let view = viewport(&input, 12, 3);
        assert_eq!(view.rows, ["  three", "  four", "  five"]);
        assert_eq!(view.first_visual_row, 2);
        assert_eq!(view.cursor_y, 2);
    }

    #[test]
    fn exact_width_end_moves_cursor_to_a_new_visual_row() {
        let mut input = InputEditor::new();
        input.insert_str("abcd");
        let view = viewport(&input, 6, 2);
        assert_eq!(view.rows, ["› abcd", "  "]);
        assert_eq!((view.cursor_x, view.cursor_y), (2, 1));
    }
}
