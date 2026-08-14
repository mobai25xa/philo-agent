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
}
