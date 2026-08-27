//! Soft-wrapped, cursor-following projection of the input editor.
//!
//! The draft has no gutter of its own: it starts directly at the composer
//! band's content column; the accent bar and surface come from the band
//! container in [`super::frame`].

use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use crate::app::input::InputEditor;
use crate::app::text;

use super::theme;

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

    let (cursor_line, cursor_byte) = input.cursor_byte();
    let mut visual_rows = Vec::new();
    let mut cursor_visual = (0, 0);

    for (logical_row, line) in input.lines().iter().enumerate() {
        let line_cursor = (logical_row == cursor_line).then_some(cursor_byte);
        let (wrapped, cursor) = wrap_line(line, line_cursor, width);
        let visual_start = visual_rows.len();
        visual_rows.extend(wrapped);
        if let Some((row, cell)) = cursor {
            cursor_visual = (visual_start + row, cell);
        }
    }

    // A draft that fits the window centers vertically in the input box
    // (placeholder included); taller drafts scroll with the cursor.
    let pad = if visual_rows.len() <= height {
        (height - visual_rows.len()) / 2
    } else {
        0
    };
    let first_visual_row = cursor_visual.0.saturating_sub(height - 1);
    let rows = vec![String::new(); pad]
        .into_iter()
        .chain(
            visual_rows
                .into_iter()
                .skip(first_visual_row)
                .take(height.saturating_sub(pad)),
        )
        .collect();
    ComposerViewport {
        rows,
        first_visual_row,
        cursor_x: cursor_visual.1.min(width.saturating_sub(1)),
        cursor_y: cursor_visual.0 + pad - first_visual_row,
    }
}

/// Draft rows wear the primary token; the placeholder stays quiet on the
/// centered row (the same row the caret would occupy). Both sit on the
/// surface painted by the frame underneath them.
pub(crate) fn styled_rows(
    view: &ComposerViewport,
    empty_placeholder: Option<&str>,
) -> Vec<Line<'static>> {
    if view.rows.is_empty() {
        return Vec::new();
    }
    let Some(placeholder) = empty_placeholder else {
        return view
            .rows
            .iter()
            .map(|row| Line::from(Span::styled(row.clone(), theme::primary())))
            .collect();
    };
    let at = view.cursor_y.min(view.rows.len().saturating_sub(1));
    view.rows
        .iter()
        .enumerate()
        .map(|(index, _)| {
            if index == at {
                Line::from(Span::styled(placeholder.to_owned(), theme::placeholder()))
            } else {
                Line::default()
            }
        })
        .collect()
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
        assert_eq!(view.rows, ["", "中文👩‍💻", "ab"]);
        assert_eq!((view.cursor_x, view.cursor_y), (2, 2));
    }

    #[test]
    fn a_draft_that_fits_centers_vertically_in_the_box() {
        let mut input = InputEditor::new();
        input.insert_str("hello");
        let view = viewport(&input, 12, 3);
        assert_eq!(view.rows, ["", "hello"]);
        assert_eq!((view.cursor_x, view.cursor_y), (5, 1));

        let empty = viewport(&InputEditor::new(), 12, 3);
        assert_eq!(empty.rows, ["", ""]);
        assert_eq!(empty.cursor_y, 1, "placeholder row is the middle line");

        let two_lines = viewport(&input, 3, 3);
        assert_eq!(
            two_lines.rows,
            ["hel", "lo"],
            "two lines leave no room to center"
        );
    }

    #[test]
    fn combining_character_occupies_one_cell() {
        let mut input = InputEditor::new();
        input.insert_str("e\u{301}x");
        let view = viewport(&input, 8, 2);
        assert_eq!(view.rows, ["e\u{301}x"]);
        assert_eq!(view.cursor_x, 2);
    }

    #[test]
    fn long_input_scrolls_inside_the_fixed_window() {
        let mut input = InputEditor::new();
        input.insert_str("one\ntwo\nthree\nfour\nfive");
        let view = viewport(&input, 12, 3);
        assert_eq!(view.rows, ["three", "four", "five"]);
        assert_eq!(view.first_visual_row, 2);
        assert_eq!(view.cursor_y, 2);
    }

    #[test]
    fn exact_width_end_moves_cursor_to_a_new_visual_row() {
        let mut input = InputEditor::new();
        input.insert_str("abcdef");
        let view = viewport(&input, 6, 2);
        assert_eq!(view.rows, ["abcdef", ""]);
        assert_eq!((view.cursor_x, view.cursor_y), (0, 1));
    }
}
