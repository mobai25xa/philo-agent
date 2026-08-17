//! Default-mode tool cards as a generic `ToolDisplay` projection.
//!
//! Session replay keeps the older `ok · {content}` summary in `session.rs`.
//! Live default cards read frozen facts (`verb`, `subject`, `body`, counts)
//! and never dump `ToolResult` content.

use philo_tools::{ToolDisplay, ToolResult};

use super::text;
use super::transcript::{TranscriptLine, compact_args, preview};

const CARD_WIDTH: usize = 120;
const KEY_WIDTH: usize = 40;
const BODY_LINES: usize = 16;
const LOCS_LINES: usize = 5;
const BODY_COLS: usize = 200;

pub(crate) fn default_card(
    tool_name: &str,
    arguments: &str,
    result: &ToolResult,
    display: Option<&ToolDisplay>,
) -> Vec<TranscriptLine> {
    let verb = fact(display, "verb").unwrap_or(tool_name);
    let subject = subject(arguments, display);
    let counts = counts(result, display);
    let mut lines = vec![line(text::truncate(
        &header(verb, subject.as_deref(), &counts),
        CARD_WIDTH,
    ))];
    if let ToolResult::Error { code, message } = result {
        lines.push(line(format!("  └ error {code} · {}", preview(message, 80))));
        return lines;
    }
    let extra = push_body(&mut lines, display);
    push_markers(&mut lines, extra, success_marker(display));
    lines
}

pub(crate) fn verbose_card(
    tool_name: &str,
    index: usize,
    batch_size: usize,
    arguments: &str,
    result: &ToolResult,
    display: Option<&ToolDisplay>,
) -> Vec<TranscriptLine> {
    let total = batch_size.max(index + 1);
    let mut lines = vec![line(format!("▸ {tool_name}  {}/{total}", index + 1))];
    if !arguments.trim().is_empty() {
        lines.push(line(format!("  args  {}", compact_args(arguments))));
    }
    match result {
        ToolResult::Success { content } => {
            lines.push(line("  ok"));
            lines.extend(content.lines().map(|row| line(format!("  {row}"))));
        }
        ToolResult::Error { code, message } => {
            lines.push(line(format!("  error {code}")));
            lines.extend(message.lines().map(|row| line(format!("  {row}"))));
        }
    }
    if let Some(display) = display {
        if !display.detail().is_empty() {
            lines.push(line("  detail"));
            lines.extend(
                display
                    .detail()
                    .lines()
                    .map(|row| line(format!("    {row}"))),
            );
        }
        if !display.facts().is_empty() {
            let facts = display
                .facts()
                .iter()
                .map(|fact| format!("{}={}", fact.name(), fact.value()))
                .collect::<Vec<_>>()
                .join("  ");
            lines.push(line(format!("  facts  {facts}")));
        }
    }
    lines
}

fn line(text: impl Into<String>) -> TranscriptLine {
    TranscriptLine {
        kind: super::transcript::LineKind::Tool,
        text: text.into(),
    }
}

fn header(verb: &str, subject: Option<&str>, counts: &str) -> String {
    let mut parts = vec![verb];
    if let Some(subject) = subject.filter(|subject| !subject.is_empty()) {
        parts.push(subject);
    }
    if !counts.is_empty() {
        parts.push(counts);
    }
    format!("• {}", parts.join("  "))
}

fn subject(arguments: &str, display: Option<&ToolDisplay>) -> Option<String> {
    if let Some(value) = fact(display, "subject") {
        let preview = preview(value, KEY_WIDTH);
        if !preview.is_empty() {
            return Some(preview);
        }
    }
    primary_key(arguments)
}

fn primary_key(arguments: &str) -> Option<String> {
    for key in ["path", "command", "pattern"] {
        if let Some(value) = json_string_field(arguments, key) {
            let preview = preview(&value, KEY_WIDTH);
            if !preview.is_empty() {
                return Some(preview);
            }
        }
    }
    None
}

fn json_string_field(raw: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = raw.find(&needle)?;
    let after = raw[start + needle.len()..].trim_start().strip_prefix(':')?;
    let after = after.trim_start().strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = after.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '"' => return Some(out),
            _ => out.push(ch),
        }
    }
    None
}

fn fact<'a>(display: Option<&'a ToolDisplay>, name: &str) -> Option<&'a str> {
    display?
        .facts()
        .iter()
        .find(|fact| fact.name() == name)
        .map(philo_tools::ToolFact::value)
}

fn counts(result: &ToolResult, display: Option<&ToolDisplay>) -> String {
    if let ToolResult::Error { code, .. } = result {
        return format!("error {code}");
    }
    let mut parts = Vec::new();
    if let (Some(start), Some(end)) = (fact(display, "start_line"), fact(display, "end_line")) {
        let mut range = format!("L{start}–L{end}");
        let shown = fact(display, "lines_shown");
        let total = fact(display, "lines_total");
        let truncated = fact(display, "truncated") == Some("true");
        if let (Some(shown), Some(total)) = (shown, total)
            && (truncated || shown != total)
        {
            range.push_str(&format!(" · {shown} of {total}"));
        }
        parts.push(range);
    }
    if let Some(n) = fact(display, "entries_total") {
        parts.push(format!("{n} entries"));
    }
    if let Some(n) = fact(display, "matches_total") {
        parts.push(format!("{n} matches"));
    }
    if fact(display, "added").is_some() || fact(display, "removed").is_some() {
        let added = fact(display, "added").unwrap_or("0");
        let removed = fact(display, "removed").unwrap_or("0");
        parts.push(format!("(+{added} -{removed})"));
    }
    if let Some(code) = fact(display, "exit_code") {
        match fact(display, "duration_ms") {
            Some(ms) => parts.push(format!("exit {code} · {}", format_ms(ms))),
            None => parts.push(format!("exit {code}")),
        }
    }
    parts.join("  ")
}

fn push_body(lines: &mut Vec<TranscriptLine>, display: Option<&ToolDisplay>) -> Option<usize> {
    let display = display?;
    match fact(Some(display), "body") {
        Some("diff") => push_capped_body(lines, display.detail(), false),
        Some("output") => push_capped_body(lines, display.detail(), true),
        Some("locs") => {
            push_locs_body(lines, display.detail());
            None
        }
        _ => None,
    }
}

fn push_capped_body(lines: &mut Vec<TranscriptLine>, source: &str, indent: bool) -> Option<usize> {
    let rows: Vec<&str> = source.lines().collect();
    if rows.is_empty() || (rows.len() == 1 && rows[0].is_empty()) {
        return None;
    }
    let extra = rows.len().saturating_sub(BODY_LINES);
    for row in rows.iter().take(BODY_LINES) {
        lines.push(line(body_row(row, indent)));
    }
    (extra > 0).then_some(extra)
}

fn push_locs_body(lines: &mut Vec<TranscriptLine>, source: &str) {
    for row in source
        .lines()
        .filter(|row| !row.trim().is_empty())
        .take(LOCS_LINES)
    {
        lines.push(line(body_row(row, true)));
    }
}

fn body_row(row: &str, indent: bool) -> String {
    let truncated = text::truncate(row, BODY_COLS);
    if indent && !truncated.starts_with("  ") {
        format!("  {truncated}")
    } else {
        truncated
    }
}

fn success_marker(display: Option<&ToolDisplay>) -> Option<String> {
    let truncated = fact(display, "truncated") == Some("true");
    let exit = fact(display, "exit_code");
    let failed_exit = exit.is_some_and(|value| value != "0");
    match (truncated, failed_exit, exit) {
        (true, true, Some(code)) => Some(format!("exit {code} · truncated")),
        (true, false, _) => Some("truncated".to_owned()),
        (false, true, Some(code)) => Some(format!("exit {code}")),
        _ => None,
    }
}

fn push_markers(lines: &mut Vec<TranscriptLine>, extra: Option<usize>, marker: Option<String>) {
    match (extra, marker) {
        (Some(n), Some(marker)) => lines.push(line(format!("  └ … +{n} lines · {marker}"))),
        (Some(n), None) => lines.push(line(format!("  └ … +{n} lines"))),
        (None, Some(marker)) => lines.push(line(format!("  └ {marker}"))),
        (None, None) => {}
    }
}

fn format_ms(ms: &str) -> String {
    let Ok(value) = ms.parse::<u64>() else {
        return format!("{ms}ms");
    };
    if value >= 1000 {
        format!("{:.1}s", value as f64 / 1000.0)
    } else {
        format!("{value}ms")
    }
}

#[cfg(test)]
mod tests {
    use philo_tools::{ToolDisplay, ToolResult};

    use super::*;

    fn texts(lines: &[TranscriptLine]) -> Vec<&str> {
        lines.iter().map(|line| line.text.as_str()).collect()
    }

    #[test]
    fn read_with_body_none_is_header_only() {
        let display = ToolDisplay::new("fn main() {}")
            .with_fact("verb", "Read")
            .with_fact("subject", "src/main.rs")
            .with_fact("body", "none")
            .with_fact("start_line", "1")
            .with_fact("end_line", "40");
        let lines = default_card(
            "read",
            r#"{"path":"src/main.rs"}"#,
            &ToolResult::success("1| fn main() {}"),
            Some(&display),
        );
        assert_eq!(texts(&lines), ["• Read  src/main.rs  L1–L40"]);
        assert!(lines.iter().all(|line| !line.text.contains("fn main")));
    }

    #[test]
    fn edit_diff_shows_plus_and_minus_lines() {
        let edit = ToolDisplay::new("-foo\n+bar")
            .with_fact("verb", "Edited")
            .with_fact("subject", "src/lib.rs")
            .with_fact("body", "diff")
            .with_fact("added", "1")
            .with_fact("removed", "1");
        let lines = default_card(
            "edit",
            r#"{"path":"src/lib.rs"}"#,
            &ToolResult::success("replaced src/lib.rs (12 → 34 bytes)"),
            Some(&edit),
        );
        assert_eq!(
            texts(&lines),
            ["• Edited  src/lib.rs  (+1 -1)", "-foo", "+bar"]
        );
        assert!(lines.iter().all(|line| !line.text.contains("replaced src")));
    }

    #[test]
    fn write_diff_shows_added_lines_not_model_confirmation() {
        let write = ToolDisplay::new("+hello\n+world")
            .with_fact("verb", "Added")
            .with_fact("subject", "src/a.rs")
            .with_fact("body", "diff")
            .with_fact("added", "2")
            .with_fact("removed", "0");
        let lines = default_card(
            "write",
            r#"{"path":"src/a.rs"}"#,
            &ToolResult::success("wrote src/a.rs (11 bytes, created)"),
            Some(&write),
        );
        assert_eq!(
            texts(&lines),
            ["• Added  src/a.rs  (+2 -0)", "+hello", "+world"]
        );
        assert!(lines.iter().all(|line| !line.text.contains("wrote src")));
    }

    #[test]
    fn grep_locs_are_capped_and_ignore_model_dump() {
        let locs = (1..=8)
            .map(|i| format!("src/lib.rs:{i}: hit {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let display = ToolDisplay::new(locs)
            .with_fact("verb", "Searched")
            .with_fact("subject", "'hit' in src")
            .with_fact("body", "locs")
            .with_fact("matches_total", "8");
        let lines = default_card(
            "grep",
            r#"{"pattern":"hit","path":"src"}"#,
            &ToolResult::success("dump of every match for the model"),
            Some(&display),
        );
        assert_eq!(
            texts(&lines),
            [
                "• Searched  'hit' in src  8 matches",
                "  src/lib.rs:1: hit 1",
                "  src/lib.rs:2: hit 2",
                "  src/lib.rs:3: hit 3",
                "  src/lib.rs:4: hit 4",
                "  src/lib.rs:5: hit 5",
            ]
        );
        assert!(
            lines
                .iter()
                .all(|line| !line.text.contains("dump of every match"))
        );
        assert!(lines.iter().all(|line| !line.text.contains("hit 8")));
    }

    #[test]
    fn list_with_body_none_is_header_only() {
        let display = ToolDisplay::new("src/main.rs\nsrc/lib.rs")
            .with_fact("verb", "Listed")
            .with_fact("subject", ".")
            .with_fact("body", "none")
            .with_fact("entries_total", "8");
        let lines = default_card(
            "list",
            r#"{"path":"."}"#,
            &ToolResult::success("src/main.rs\nsrc/lib.rs"),
            Some(&display),
        );
        assert_eq!(texts(&lines), ["• Listed  .  8 entries"]);
        assert!(lines.iter().all(|line| !line.text.contains("src/main.rs")));
    }

    #[test]
    fn shell_output_is_indented_and_nonzero_exit_keeps_a_footer() {
        let ok = ToolDisplay::new("ok\npassed")
            .with_fact("verb", "Ran")
            .with_fact("subject", "cargo test")
            .with_fact("body", "output")
            .with_fact("exit_code", "0")
            .with_fact("duration_ms", "1200");
        let lines = default_card(
            "shell",
            r#"{"command":"cargo test"}"#,
            &ToolResult::success("exit_code: 0\nok"),
            Some(&ok),
        );
        assert_eq!(
            texts(&lines),
            ["• Ran  cargo test  exit 0 · 1.2s", "  ok", "  passed"]
        );

        let failed = ToolDisplay::new("boom")
            .with_fact("verb", "Ran")
            .with_fact("subject", "cargo test")
            .with_fact("body", "output")
            .with_fact("exit_code", "1")
            .with_fact("duration_ms", "1200");
        let lines = default_card(
            "shell",
            r#"{"command":"cargo test"}"#,
            &ToolResult::success("exit_code: 1\nbloom"),
            Some(&failed),
        );
        assert_eq!(
            texts(&lines),
            ["• Ran  cargo test  exit 1 · 1.2s", "  boom", "  └ exit 1",]
        );
        assert!(lines.iter().all(|line| !line.text.contains("bloom")));
    }

    #[test]
    fn error_is_two_lines() {
        let error = default_card(
            "edit",
            r#"{"path":"src/lib.rs"}"#,
            &ToolResult::error("not_unique", "3 matches"),
            None,
        );
        assert_eq!(
            texts(&error),
            [
                "• edit  src/lib.rs  error not_unique",
                "  └ error not_unique · 3 matches",
            ]
        );
    }

    #[test]
    fn output_body_is_capped_at_sixteen_lines() {
        let detail = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let display = ToolDisplay::new(detail)
            .with_fact("verb", "Ran")
            .with_fact("subject", "seq")
            .with_fact("body", "output")
            .with_fact("exit_code", "0")
            .with_fact("truncated", "true");
        let lines = default_card(
            "shell",
            r#"{"command":"seq"}"#,
            &ToolResult::success("model dump 20"),
            Some(&display),
        );
        assert_eq!(lines[0].text, "• Ran  seq  exit 0");
        assert_eq!(lines[1].text, "  line 1");
        assert_eq!(lines[16].text, "  line 16");
        assert_eq!(lines[17].text, "  └ … +4 lines · truncated");
        assert_eq!(lines.len(), 18);
        assert!(lines.iter().all(|line| !line.text.contains("line 20")));
        assert!(lines.iter().all(|line| !line.text.contains("model dump")));
    }

    #[test]
    fn missing_body_fact_never_dumps_display_detail() {
        let display =
            ToolDisplay::new("read completed\nfull display detail").with_fact("bytes", "840");
        let lines = default_card(
            "read_file",
            r#"{"path":"src/main.rs"}"#,
            &ToolResult::success("fn main() {}"),
            Some(&display),
        );
        assert_eq!(texts(&lines), ["• read_file  src/main.rs"]);
        assert!(
            lines
                .iter()
                .all(|line| !line.text.contains("full display detail"))
        );
    }
}
