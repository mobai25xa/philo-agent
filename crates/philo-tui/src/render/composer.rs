//! Soft-wrapped, cursor-following projection of the input editor into the
//! v4.0 rounded box (P2 task §3).
//!
//! The draft window rides one row inside the borders: single-row drafts
//! show a 3-row outer box, multi-line drafts grow it toward
//! [`theme::COMPOSER_MAX_OUTER`], and taller drafts scroll internally with
//! an `[L{top}/{total}]` marker riding the bottom border's right side.

use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use crate::app::input::InputEditor;
use crate::app::text;

use super::theme;

/// Outer height (borders included) the composer box needs right now: 3 for
/// a single visual row, growing one-for-one with wrapped draft rows up to
/// the 8-row cap.
pub(crate) fn outer_height(draft_wrapped_lines: usize) -> u16 {
    let inner = draft_wrapped_lines.max(1);
    u16::try_from(inner)
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .clamp(theme::COMPOSER_MIN_OUTER, theme::COMPOSER_MAX_OUTER)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ComposerViewport {
    pub(crate) rows: Vec<String>,
    /// Index of `rows[0]` within the wrapped visual rows (`[L{n}/{total}]`).
    pub(crate) first_visual_row: usize,
    /// Total wrapped visual rows of the whole draft.
    pub(crate) total_visual_rows: usize,
    pub(crate) cursor_x: usize,
    pub(crate) cursor_y: usize,
    pub(crate) empty: bool,
}

impl ComposerViewport {
    /// Whether the internal scroller is engaged (draft exceeds the box).
    pub(crate) fn scrolls(&self, height: usize) -> bool {
        self.total_visual_rows > height
    }
}

/// Wraps the draft and chooses the visible window of `height` rows under
/// the cursor-follow policy. `pad` centers short drafts like v3 did; the
/// internal scroll pins the cursor row into view when content overflows.
pub(crate) fn viewport(input: &InputEditor, width: usize, height: usize) -> ComposerViewport {
    if width == 0 || height == 0 {
        return ComposerViewport {
            rows: Vec::new(),
            first_visual_row: 0,
            total_visual_rows: 0,
            cursor_x: 0,
            cursor_y: 0,
            empty: input.is_empty(),
        };
    }

    let (cursor_line, cursor_byte) = input.cursor_byte();
    let mut visual_rows = Vec::new();
    let mut cursor_visual = (0usize, 0usize);

    for (logical_row, line) in input.lines().iter().enumerate() {
        let line_cursor = (logical_row == cursor_line).then_some(cursor_byte);
        let (wrapped, cursor) = wrap_line(line, line_cursor, width);
        let visual_start = visual_rows.len();
        visual_rows.extend(wrapped);
        if let Some((row, cell)) = cursor {
            cursor_visual = (visual_start + row, cell);
        }
    }

    let total = visual_rows.len();
    if total <= height {
        // Fits: center vertically so single drafts ride the middle line.
        let pad = (height - total) / 2;
        let rows = pad_blank(pad)
            .chain(visual_rows)
            .chain(std::iter::repeat(String::new()))
            .take(height)
            .collect();
        ComposerViewport {
            rows,
            first_visual_row: 0,
            total_visual_rows: total,
            cursor_x: cursor_visual.1.min(width.saturating_sub(1)),
            cursor_y: cursor_visual.0 + pad,
            empty: input.is_empty(),
        }
    } else {
        // Overflow: keep the cursor row visible; keep some lookahead below
        // it when possible.
        let first = cursor_visual.0.saturating_sub(height.saturating_sub(1));
        let first = first.min(total - height);
        let rows = visual_rows
            [first..first + height]
            .to_vec();
        ComposerViewport {
            rows,
            first_visual_row: first,
            total_visual_rows: total,
            cursor_x: cursor_visual.1.min(width.saturating_sub(1)),
            cursor_y: cursor_visual.0 - first,
            empty: input.is_empty(),
        }
    }
}

fn pad_blank(pad: usize) -> impl Iterator<Item = String> {
    std::iter::repeat_with(String::new).take(pad)
}

/// Draft/placeholder rows wear their paint tokens here so every composed
/// line carries the footer background instead of falling back to default.
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
                Line::from(Span::styled(
                    placeholder.to_owned(),
                    theme::placeholder(),
                ))
            } else {
                Line::default()
            }
        })
        .collect()
}

/// P2 paint path: plain draft rows on the footer band (the band already
/// carries FOOTER_BG; a centered empty draft renders nothing). The frame's
/// placeholder glyph comes from the prompt column, so an empty draft simply
/// paints blank rows.
pub(crate) fn styled_rows_painted(view: &ComposerViewport) -> Vec<Line<'static>> {
    styled_rows(view, None)
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
        assert_eq!(view.rows, ["", "中文👩‍💻", "ab", ""]);
        assert_eq!((view.cursor_x, view.cursor_y), (2, 2));
        assert_eq!(view.total_visual_rows, 2);
    }

    #[test]
    fn a_draft_that_fits_centers_vertically_in_the_box() {
        let mut input = InputEditor::new();
        input.insert_str("hello");
        let view = viewport(&input, 12, 3);
        assert_eq!(view.rows, ["", "hello", ""]);
        assert_eq!((view.cursor_x, view.cursor_y), (5, 1));

        let empty = viewport(&InputEditor::new(), 12, 3);
        assert_eq!(empty.rows, ["", "", ""]);
        assert_eq!(empty.cursor_y, 1, "placeholder row is the middle line");

        let two_lines = viewport(&input, 3, 3);
        assert_eq!(
            two_lines.rows,
            ["hel", "lo", ""],
            "two lines leave no room to center"
        );
    }

    #[test]
    fn combining_character_occupies_one_cell() {
        let mut input = InputEditor::new();
        input.insert_str("e\u{301}x");
        let view = viewport(&input, 8, 2);
        assert_eq!(view.rows, ["e\u{301}x", ""]);
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
        assert!(view.scrolls(3));
    }

    #[test]
    fn exact_width_end_moves_cursor_to_a_new_visual_row() {
        let mut input = InputEditor::new();
        input.insert_str("abcdef");
        let view = viewport(&input, 6, 4);
        assert_eq!(view.rows, ["", "abcdef", "", ""]);
        assert_eq!((view.cursor_x, view.cursor_y), (0, 2));
    }

    #[test]
    fn outer_height_ladder_follows_the_wrapped_draft() {
        assert_eq!(outer_height(0), theme::COMPOSER_MIN_OUTER);
        assert_eq!(outer_height(1), theme::COMPOSER_MIN_OUTER);
        assert_eq!(outer_height(2), 4);
        assert_eq!(outer_height(5), 7);
        assert_eq!(outer_height(6), theme::COMPOSER_MAX_OUTER);
        assert_eq!(outer_height(7), theme::COMPOSER_MAX_OUTER);
        assert_eq!(outer_height(24), theme::COMPOSER_MAX_OUTER);
    }

    #[test]
    fn cursor_visibility_drives_the_internal_scroll_offset() {
        // 24 wrapped rows in a 6-row window: typing at the very end keeps
        // the last rows on screen.
        let mut input = InputEditor::new();
        for index in 0..24 {
            if index > 0 {
                input.insert_newline();
            }
            input.insert_str(&format!("line-{index}"));
        }
        let view = viewport(&input, 40, 6);
        assert_eq!(view.rows.first().map(String::as_str), Some("line-18"));
        assert_eq!(view.first_visual_row, 18);
        assert_eq!(
            view.cursor_y,
            5,
            "the caret sits on the last inner row at the tail"
        );
        assert_eq!(view.total_visual_rows, 24);
    }
}
