//! Frozen display-channel vocabulary shared by the six standard tools.
//!
//! Every successful display carries the v0.38 card facts: `title` (the
//! present-tense card name: `Read|Grep|List Directory|Edit|Write|Run`),
//! repeatable `subject` rows (paths / patterns / commands), an optional
//! `count` phrase (`1 file`, `1 search`, `1 command`), an optional `result`
//! phrase (`Succeeded. File edited.` / `exit 0 · 4.2s`), and `body` (the
//! body kind). The legacy `verb` fact is still emitted for the transition
//! and is dropped once the TUI card renderer consumes `title` (redesign
//! T5.1). Auxiliary numeric facts (line ranges, byte totals, truncation)
//! stay per-tool open key-values. Detail is the optional body text (`none`
//! → empty, `diff` → hunk / plus-lines, `locs` → `path:line`, `output` →
//! bounded command text). Model-channel bytes are assembled by the tools
//! themselves and must not change here.

use philo_tools::ToolDisplay;

/// Display-channel cap for write `+` lines.
pub(crate) const MAX_PLUS_DISPLAY_LINES: usize = 80;

const EDIT_CONTEXT_LINES: usize = 2;

/// Starts a display payload with the required `title` and `body` facts plus
/// the transitional legacy `verb`. Callers chain [`CardFacts`] helpers and
/// auxiliary numeric facts.
pub(crate) fn card(
    title: &str,
    legacy_verb: &str,
    body: &str,
    detail: impl Into<String>,
) -> ToolDisplay {
    ToolDisplay::new(detail.into())
        .with_fact("title", title)
        .with_fact("verb", legacy_verb)
        .with_fact("body", body)
}

/// Fluent helpers for the v0.38 card fact vocabulary.
pub(crate) trait CardFacts: Sized {
    /// Appends one repeatable subject row.
    fn subject(self, value: impl Into<String>) -> Self;
    /// Sets the header count phrase (number + unit).
    fn count(self, phrase: impl Into<String>) -> Self;
    /// Sets the success result phrase.
    fn result(self, phrase: impl Into<String>) -> Self;
}

impl CardFacts for ToolDisplay {
    fn subject(self, value: impl Into<String>) -> Self {
        self.with_fact("subject", value)
    }
    fn count(self, phrase: impl Into<String>) -> Self {
        self.with_fact("count", phrase)
    }
    fn result(self, phrase: impl Into<String>) -> Self {
        self.with_fact("result", phrase)
    }
}

/// Formats elapsed milliseconds as a compact result-phrase duration
/// (`4.2s` from one second up, plain milliseconds below).
pub(crate) fn format_elapsed_ms(ms: u128) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

/// Write-tool detail: each content line as `+{line}`, capped for display.
pub(crate) fn plus_lines(content: &str, max_lines: usize) -> (String, usize, bool) {
    let mut added = 0usize;
    let mut detail = String::new();
    let mut truncated = false;
    for line in content.lines() {
        if added < max_lines {
            if added > 0 {
                detail.push('\n');
            }
            detail.push('+');
            detail.push_str(line);
        } else {
            truncated = true;
        }
        added += 1;
    }
    (detail, added, truncated)
}

/// Unified hunk for a unique `old_string` replacement: an `@@ -a,b +a,c @@`
/// header (the redesign T5.2 gutter derives old/new line numbers from it),
/// ±2 file context lines, then `-` old lines, `+` new lines. No `---` /
/// `+++` file headers.
pub(crate) fn edit_hunk(text: &str, old_string: &str, new_string: &str) -> (String, usize, usize) {
    let Some(start) = text.find(old_string) else {
        return (String::new(), 0, 0);
    };
    let old_end = start + old_string.len();
    let lines = numbered_lines(text);
    if lines.is_empty() {
        return (String::new(), 0, 0);
    }

    let first = line_index_containing(&lines, start);
    let last = line_index_containing(&lines, old_end.saturating_sub(1));
    let line_start = lines[first].0;
    let line_end = if last + 1 < lines.len() {
        lines[last + 1].0
    } else {
        text.len()
    };

    let new_block = format!(
        "{}{}{}",
        &text[line_start..start],
        new_string,
        &text[old_end..line_end]
    );

    let before = first.saturating_sub(EDIT_CONTEXT_LINES)..first;
    let after_end = (last + 1 + EDIT_CONTEXT_LINES).min(lines.len());
    let context = before.len() + (after_end - (last + 1));
    let removed = last + 1 - first;
    let added = new_block.lines().count();

    let mut rows = Vec::new();
    // Leading context is shared, so both sides start at the same number.
    let hunk_start = first + 1;
    rows.push(format!(
        "@@ -{hunk_start},{old_count} +{hunk_start},{new_count} @@",
        old_count = context + removed,
        new_count = context + added,
    ));
    rows.extend(before.map(|index| format!("  {}", lines[index].1)));
    rows.extend(
        lines[first..=last]
            .iter()
            .map(|(_, line)| format!("-{line}")),
    );
    for line in new_block.lines() {
        rows.push(format!("+{line}"));
    }
    rows.extend((last + 1..after_end).map(|index| format!("  {}", lines[index].1)));
    (rows.join("\n"), added, removed)
}

fn numbered_lines(text: &str) -> Vec<(usize, &str)> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'\n' {
            lines.push((start, &text[start..index]));
            index += 1;
            start = index;
        } else if bytes[index] == b'\r' {
            lines.push((start, &text[start..index]));
            index += 1;
            if bytes.get(index) == Some(&b'\n') {
                index += 1;
            }
            start = index;
        } else {
            index += 1;
        }
    }
    if start < bytes.len() {
        lines.push((start, &text[start..]));
    }
    lines
}

fn line_index_containing(lines: &[(usize, &str)], offset: usize) -> usize {
    lines
        .iter()
        .rposition(|(start, _)| *start <= offset)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_emits_the_v038_vocabulary_with_legacy_verb() {
        let display = card("Read", "Read", "none", "")
            .subject("src/main.rs")
            .count("1 file")
            .result("Succeeded. File read.");
        let names: Vec<&str> = display.facts().iter().map(|fact| fact.name()).collect();
        assert_eq!(
            names,
            ["title", "verb", "body", "subject", "count", "result"]
        );
        assert_eq!(display.facts()[0].value(), "Read");
        assert_eq!(display.facts()[3].value(), "src/main.rs");
        assert_eq!(display.facts()[4].value(), "1 file");
    }

    #[test]
    fn elapsed_formats_seconds_from_one_second_up() {
        assert_eq!(format_elapsed_ms(420), "420ms");
        assert_eq!(format_elapsed_ms(1000), "1.0s");
        assert_eq!(format_elapsed_ms(4200), "4.2s");
    }

    #[test]
    fn edit_hunk_replaces_a_partial_line_with_following_context() {
        let text = "fn old_name() {}\ncall(old_value);\n";
        let (hunk, added, removed) = edit_hunk(text, "fn old_name()", "fn new_name()");
        assert_eq!(
            hunk,
            "@@ -1,2 +1,2 @@\n-fn old_name() {}\n+fn new_name() {}\n  call(old_value);"
        );
        assert_eq!((added, removed), (1, 1));
    }

    #[test]
    fn edit_hunk_keeps_two_lines_of_file_context() {
        let text = "a\nb\nc\nOLD\nd\ne\nf\n";
        let (hunk, added, removed) = edit_hunk(text, "OLD", "NEW");
        assert_eq!(
            hunk,
            "@@ -4,5 +4,5 @@\n  b\n  c\n-OLD\n+NEW\n  d\n  e"
        );
        assert_eq!((added, removed), (1, 1));
    }

    #[test]
    fn plus_lines_caps_detail_and_counts_all_rows() {
        let content = (0..90)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (detail, added, truncated) = plus_lines(&content, MAX_PLUS_DISPLAY_LINES);
        assert_eq!(added, 90);
        assert!(truncated);
        assert_eq!(detail.lines().count(), MAX_PLUS_DISPLAY_LINES);
        assert!(detail.starts_with("+l0\n"));
        assert!(detail.contains("+l79"));
        assert!(!detail.contains("+l80"));
    }
}
