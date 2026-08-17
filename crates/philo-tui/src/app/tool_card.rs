//! Default-mode tool cards for live transcript lines.
//!
//! Session replay keeps the older `ok · {content}` summary in `session.rs`.
//! These cards prefer `ToolDisplay` facts and a single primary argument.

use philo_tools::{ToolDisplay, ToolResult};

use super::text;
use super::transcript::{TranscriptLine, compact_args, preview};

const CARD_WIDTH: usize = 120;
const KEY_WIDTH: usize = 40;
const DETAIL_WIDTH: usize = 40;

pub(crate) fn default_card(
    tool_name: &str,
    arguments: &str,
    result: &ToolResult,
    display: Option<&ToolDisplay>,
) -> Vec<TranscriptLine> {
    let key = primary_key(arguments);
    let metric = metric(tool_name, result, display);
    let rest = match key {
        Some(key) => format!("{key}  {metric}"),
        None => metric,
    };
    let mut lines = vec![line(text::truncate(
        &format!("▸ {tool_name}  {rest}"),
        CARD_WIDTH,
    ))];
    if let Some(reason) = second_line(result, display) {
        lines.push(line(reason));
    }
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

fn primary_key(arguments: &str) -> Option<String> {
    for key in ["path", "command", "pattern", "old_string"] {
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

fn metric(tool_name: &str, result: &ToolResult, display: Option<&ToolDisplay>) -> String {
    if let ToolResult::Error { code, .. } = result {
        return format!("error {code}");
    }
    match tool_name {
        "read" | "read_file" => read_metric(display),
        "list" => count_metric(display, "entries_total", "entries"),
        "grep" => count_metric(display, "matches_total", "matches"),
        "write" => write_metric(display),
        "edit" => edit_metric(display),
        "shell" => shell_metric(display),
        _ => display_or_ok(display),
    }
}

fn read_metric(display: Option<&ToolDisplay>) -> String {
    if let Some(total) = fact(display, "lines_total") {
        return format!("{total} lines");
    }
    if let Some(bytes) = fact(display, "bytes").or_else(|| fact(display, "bytes_total")) {
        return format!("{bytes} B");
    }
    display_or_ok(display)
}

fn count_metric(display: Option<&ToolDisplay>, name: &str, unit: &str) -> String {
    fact(display, name).map_or_else(|| display_or_ok(display), |count| format!("{count} {unit}"))
}

fn write_metric(display: Option<&ToolDisplay>) -> String {
    let operation = fact(display, "operation").unwrap_or("wrote");
    match fact(display, "bytes") {
        Some(bytes) => format!("{operation} · {bytes} B"),
        None => operation.to_owned(),
    }
}

fn edit_metric(display: Option<&ToolDisplay>) -> String {
    match (fact(display, "bytes_before"), fact(display, "bytes_after")) {
        (Some(before), Some(after)) => format!("replaced · {before}→{after} B"),
        _ => "replaced".to_owned(),
    }
}

fn shell_metric(display: Option<&ToolDisplay>) -> String {
    let exit = fact(display, "exit_code").unwrap_or("0");
    match fact(display, "duration_ms") {
        Some(ms) => format!("exit {exit} · {}", format_ms(ms)),
        None => format!("exit {exit}"),
    }
}

fn display_or_ok(display: Option<&ToolDisplay>) -> String {
    display
        .map(ToolDisplay::detail)
        .map(|detail| preview(detail, DETAIL_WIDTH))
        .filter(|detail| !detail.is_empty())
        .unwrap_or_else(|| "ok".to_owned())
}

fn second_line(result: &ToolResult, display: Option<&ToolDisplay>) -> Option<String> {
    match result {
        ToolResult::Error { code, message } => {
            Some(format!("  └ error {code} · {}", preview(message, 80)))
        }
        ToolResult::Success { .. } => {
            let truncated = fact(display, "truncated") == Some("true");
            let exit = fact(display, "exit_code");
            let failed_exit = exit.is_some_and(|value| value != "0");
            match (truncated, failed_exit, exit) {
                (true, true, Some(code)) => Some(format!("  └ exit {code} · truncated")),
                (true, false, _) => Some("  └ truncated".to_owned()),
                (false, true, Some(code)) => Some(format!("  └ exit {code}")),
                _ => None,
            }
        }
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

    #[test]
    fn success_is_one_fact_line_without_model_content() {
        let display = ToolDisplay::new("read src/main.rs: 12 of 40 lines shown")
            .with_fact("lines_total", "40")
            .with_fact("truncated", "false");
        let lines = default_card(
            "read",
            r#"{"path":"src/main.rs"}"#,
            &ToolResult::success("1| fn main() {}"),
            Some(&display),
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "▸ read  src/main.rs  40 lines");
        assert!(!lines[0].text.contains("fn main"));
    }

    #[test]
    fn error_and_truncation_use_a_second_line() {
        let error = default_card(
            "edit",
            r#"{"path":"src/lib.rs"}"#,
            &ToolResult::error("not_unique", "3 matches"),
            None,
        );
        assert_eq!(
            error
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            [
                "▸ edit  src/lib.rs  error not_unique",
                "  └ error not_unique · 3 matches",
            ]
        );

        let truncated = ToolDisplay::new("read truncated")
            .with_fact("lines_total", "40")
            .with_fact("truncated", "true");
        let lines = default_card(
            "read",
            r#"{"path":"src/main.rs"}"#,
            &ToolResult::success("body"),
            Some(&truncated),
        );
        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["▸ read  src/main.rs  40 lines", "  └ truncated"]
        );
    }

    #[test]
    fn nonzero_shell_exit_is_a_two_line_card() {
        let display = ToolDisplay::new("boom")
            .with_fact("exit_code", "1")
            .with_fact("duration_ms", "1200");
        let lines = default_card(
            "shell",
            r#"{"command":"cargo test"}"#,
            &ToolResult::success("exit_code: 1\nbloom"),
            Some(&display),
        );
        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["▸ shell  cargo test  exit 1 · 1.2s", "  └ exit 1",]
        );
    }
}
