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

/// Soft-wraps a hanging-indented tool line (`• ` / `  ` / `  └ ` / `+ ` / `- `).
pub(crate) fn wrap_hanging(text: &str, max_width: usize) -> Vec<String> {
    if let Some(rest) = text.strip_prefix("  └ ") {
        wrap_prefixed(rest, max_width, "  └ ", "    ")
    } else if let Some(rest) = text.strip_prefix("    ") {
        wrap_prefixed(rest, max_width, "    ", "    ")
    } else if let Some(rest) = text.strip_prefix("  ") {
        wrap_prefixed(rest, max_width, "  ", "  ")
    } else if let Some(rest) = text.strip_prefix("• ") {
        wrap_prefixed(rest, max_width, "• ", "  ")
    } else if let Some(rest) = text.strip_prefix('+') {
        wrap_prefixed(
            rest.strip_prefix(' ').unwrap_or(rest),
            max_width,
            "+ ",
            "+ ",
        )
    } else if let Some(rest) = text.strip_prefix('-') {
        wrap_prefixed(
            rest.strip_prefix(' ').unwrap_or(rest),
            max_width,
            "- ",
            "- ",
        )
    } else {
        wrap(text, max_width)
    }
}

/// Soft-wraps a reasoning cell. The `think` header is ordinary wrap; body
/// rows hang with a `│ ` gutter (U+2502 + space) and never write that bar
/// back into the cell store.
pub(crate) fn wrap_reasoning(text: &str, max_width: usize) -> Vec<String> {
    if text == "think" {
        wrap(text, max_width)
    } else {
        let rest = text.strip_prefix("  ").unwrap_or(text);
        wrap_prefixed(rest, max_width, "│ ", "│ ")
    }
}

fn wrap_prefixed(
    text: &str,
    max_width: usize,
    first_gutter: &str,
    rest_gutter: &str,
) -> Vec<String> {
    if max_width == 0 {
        return Vec::new();
    }
    if text.is_empty() {
        return vec![first_gutter.to_owned()];
    }
    let content_width = max_width.saturating_sub(width(first_gutter)).max(1);
    wrap(text, content_width)
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let gutter = if index == 0 {
                first_gutter
            } else {
                rest_gutter
            };
            format!("{gutter}{row}")
        })
        .collect()
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
    fn wrap_keeps_cjk_cells() {
        assert_eq!(wrap("中文ab", 4), ["中文", "ab"]);
        assert_eq!(slice_columns("中文ab", 0, 4), "中文");
        assert_eq!(slice_columns("中文ab", 2, 6), "文ab");
        assert_eq!(slice_columns("abc", 1, 1), "");
    }

    #[test]
    fn wrap_hanging_keeps_tool_indent() {
        assert_eq!(wrap_hanging("  abcdefgh", 6), ["  abcd", "  efgh"]);
        assert_eq!(wrap_hanging("  └ abcdef", 8), ["  └ abcd", "    ef"]);
        assert_eq!(wrap_hanging("• abcdefgh", 6), ["• abcd", "  efgh"]);
        assert_eq!(wrap_hanging("+abcdefgh", 5), ["+ abc", "+ def", "+ gh"]);
        assert_eq!(wrap_hanging("-abcdefgh", 5), ["- abc", "- def", "- gh"]);
        assert_eq!(wrap_hanging("+ foo", 8), ["+ foo"]);
        assert_eq!(wrap_hanging("- old", 8), ["- old"]);
    }

    #[test]
    fn wrap_reasoning_hangs_body_with_a_bar() {
        assert_eq!(wrap_reasoning("think", 20), ["think"]);
        assert_eq!(
            wrap_reasoning("  abcdefghijkl", 6),
            ["│ abcd", "│ efgh", "│ ijkl"]
        );
    }
}
