//! Grapheme-safe helpers for text projected into terminal cells.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(crate) fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub(crate) fn truncate(text: &str, max_width: usize) -> String {
    if width(text) <= max_width {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }

    const ELLIPSIS: &str = "...";
    let suffix = if max_width >= width(ELLIPSIS) {
        ELLIPSIS
    } else {
        ""
    };
    let content_width = max_width.saturating_sub(width(suffix));
    let mut result = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = width(grapheme);
        if used + grapheme_width > content_width {
            break;
        }
        result.push_str(grapheme);
        used += grapheme_width;
    }
    result.push_str(suffix);
    result
}

#[cfg(test)]
pub(crate) fn tail(text: &str, max_width: usize) -> String {
    if width(text) <= max_width {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }

    const PREFIX: &str = "...";
    let prefix = if max_width >= width(PREFIX) {
        PREFIX
    } else {
        ""
    };
    let content_width = max_width.saturating_sub(width(prefix));
    let mut kept = Vec::new();
    let mut used = 0;
    for grapheme in text.graphemes(true).rev() {
        let grapheme_width = width(grapheme);
        if used + grapheme_width > content_width {
            break;
        }
        kept.push(grapheme);
        used += grapheme_width;
    }
    kept.reverse();
    format!("{prefix}{}", kept.concat())
}

/// Soft-wraps `text` on terminal cells without splitting graphemes.
pub(crate) fn wrap(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for logical in text.split('\n') {
        if logical.is_empty() {
            rows.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut current_width = 0;
        for grapheme in logical.graphemes(true) {
            let grapheme_width = width(grapheme);
            if current_width > 0 && current_width + grapheme_width > max_width {
                rows.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push_str(grapheme);
            current_width += grapheme_width;
        }
        rows.push(current);
    }
    rows
}

/// Inclusive-start, exclusive-end slice of `text` in terminal columns.
/// Never splits a grapheme; a grapheme that straddles a bound is kept if
/// it starts inside the range.
pub(crate) fn slice_columns(text: &str, start: usize, end: usize) -> String {
    if start >= end {
        return String::new();
    }
    let mut result = String::new();
    let mut col = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = width(grapheme);
        let next = col + grapheme_width;
        if col >= end {
            break;
        }
        if next > start {
            result.push_str(grapheme);
        }
        col = next;
    }
    result
}

/// Last `height` visual rows of `text` after wrapping to `max_width`.
pub(crate) fn tail_rows(text: &str, max_width: usize, height: usize) -> Vec<String> {
    if height == 0 || max_width == 0 {
        return Vec::new();
    }
    let rows = wrap(text, max_width);
    let skip = rows.len().saturating_sub(height);
    rows.into_iter().skip(skip).collect()
}

pub(crate) fn pad(text: &str, target_width: usize) -> String {
    let mut result = truncate(text, target_width);
    result.extend(std::iter::repeat_n(
        ' ',
        target_width.saturating_sub(width(&result)),
    ));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_and_tail_respect_terminal_cells_and_graphemes() {
        assert_eq!(truncate("ab中文cd", 7), "ab中...");
        assert_eq!(tail("ab中文cd", 7), "...文cd");
        assert_eq!(truncate("e\u{301}x", 1), "e\u{301}");
    }

    #[test]
    fn padding_uses_cells_instead_of_scalar_count() {
        assert_eq!(pad("中", 4), "中  ");
        assert_eq!(width(&pad("中", 4)), 4);
    }

    #[test]
    fn wrap_and_tail_rows_keep_cjk_cells() {
        assert_eq!(wrap("中文ab", 4), ["中文", "ab"]);
        assert_eq!(tail_rows("one\ntwo\nthree", 8, 2), ["two", "three"]);
        assert_eq!(slice_columns("中文ab", 0, 4), "中文");
        assert_eq!(slice_columns("中文ab", 2, 6), "文ab");
        assert_eq!(slice_columns("abc", 1, 1), "");
    }
}
