//! Default-mode tool cards as a generic `FrontendToolDisplay` projection.
//!
//! Cards are sequences of `Tool` cells whose [`Tone`] carries the paint
//! structure (design §3.3): a `Title` header opens the card, `Detail` rows
//! carry the `↳` details, `Failure` marks the red failure row, and diff
//! bodies use `DiffDel`/`DiffIns` so the shell washes their background.
//! The TUI holds zero tool knowledge — everything comes from the frozen
//! facts vocabulary (`title` / repeatable `subject` / `count` / `result` /
//! `body`) supplied by tools-std.
//!
//! Session replay keeps the older `ok · {content}` summary in `session.rs`.
//! Verbose mode keeps its structure (full args, model-facing result,
//! detail/facts) and only swaps tokens.

use philo_agent_service::{FrontendToolDisplay, FrontendToolResult};

use super::text;
use super::transcript::{LineKind, Tone, TranscriptLine, compact_args, preview};
use crate::render::theme::DETAIL;

const CARD_WIDTH: usize = 120;
const KEY_WIDTH: usize = 40;
const BODY_LINES: usize = 16;
const LOCS_LINES: usize = 5;
const BODY_COLS: usize = 200;

pub(crate) fn default_card(
    tool_name: &str,
    arguments: &str,
    result: &FrontendToolResult,
    display: Option<&FrontendToolDisplay>,
) -> Vec<TranscriptLine> {
    let title = fact(display, "title").unwrap_or(tool_name);
    let subjects: Vec<String> = display
        .map(|display| {
            display
                .facts
                .iter()
                .filter(|(name, _)| name.as_str() == "subject")
                .map(|(_, value)| value.clone())
                .collect()
        })
        .unwrap_or_default();
    let count = fact(display, "count").filter(|count| !count.is_empty());
    // Count-bearing headers list their subjects below (`Read 2 files` +
    // four paths); a subject header already names its target and never
    // repeats it (`Edit src/app.rs`).
    let header_used_subject = count.is_none() && !subjects.is_empty();
    let mut lines = vec![header_line(title, count, &subjects, arguments)];
    if let FrontendToolResult::Error { code, message } = result {
        lines.push(card_line(
            format!("  ↳ Failed. {code} · {}", preview(message, 80)),
            Tone::Failure,
        ));
        return lines;
    }
    if !header_used_subject {
        push_subjects(&mut lines, &subjects);
    }
    if let Some(result) = fact(display, "result") {
        lines.push(card_line(format!("  ↳ {result}"), Tone::Detail));
    }
    let body_kind = fact(display, "body");
    let extra = push_body(&mut lines, display, body_kind);
    if let Some(extra) = extra.filter(|_| body_kind != Some("locs")) {
        lines.push(card_line(format!("  ↳ … +{extra} lines"), Tone::Detail));
    }
    lines
}

/// Repeatable subject rows: the first carries the `↳` prefix, continuations
/// align under it.
fn push_subjects(lines: &mut Vec<TranscriptLine>, subjects: &[String]) {
    for (index, subject) in subjects.iter().enumerate() {
        if index == 0 {
            lines.push(card_line(format!("  {DETAIL} {subject}"), Tone::Detail));
        } else {
            lines.push(card_line(format!("    {subject}"), Tone::Detail));
        }
    }
}

/// `{title} {count}`, else `{title} {subject}`, else the bare display name.
/// Without facts the primary argument key stands in for the subject so even
/// undisplayed failures anchor to their target.
fn header_line(
    title: &str,
    count: Option<&str>,
    subjects: &[String],
    arguments: &str,
) -> TranscriptLine {
    let rest = match count {
        Some(count) => Some(count.to_owned()),
        None => match subjects.first() {
            Some(subject) => Some(preview(subject, KEY_WIDTH)),
            None => primary_key(arguments),
        },
    };
    let text = match rest {
        Some(rest) if !rest.is_empty() => format!("{title} {rest}"),
        _ => title.to_owned(),
    };
    card_line(text::truncate(&text, CARD_WIDTH), Tone::Title)
}

fn push_body(
    lines: &mut Vec<TranscriptLine>,
    display: Option<&FrontendToolDisplay>,
    body_kind: Option<&str>,
) -> Option<usize> {
    let display = display?;
    let rows = match body_kind {
        Some("diff") => diff_rows(&display.detail),
        Some("output") => indented_rows(&display.detail, BODY_LINES),
        Some("locs") => indented_rows(&display.detail, LOCS_LINES),
        _ => return None,
    };
    let extra = rows.extra;
    if !rows.lines.is_empty() {
        // One blank row between the details and the body (design §3.3).
        lines.push(card_line("", Tone::Plain));
        lines.extend(rows.lines);
    }
    extra
}

struct BodyRows {
    lines: Vec<TranscriptLine>,
    /// Hidden rows beyond the cap, reported by the `… +N lines` footer.
    extra: Option<usize>,
}

/// Diff body with a right-aligned line-number gutter: deletions carry their
/// old number, insertions and context their new one, both derived from the
/// unified hunk header (`@@ -a,b +a,c @@`). Write-style `+` blocks have no
/// header and number from 1. The header itself never renders.
fn diff_rows(source: &str) -> BodyRows {
    let mut state: Option<(usize, usize)> = None;
    let rendered: Vec<TranscriptLine> = source
        .lines()
        .filter_map(|row| numbered_diff_row(row, &mut state))
        .map(|(tone, number, content)| {
            card_line(format!("    {} | {content}", number.unwrap_or(0)), tone)
        })
        .collect();
    finish(rendered, BODY_LINES)
}

/// Classifies one hunk row against the rolling `(old_next, new_next)`
/// counters seeded by the hunk header: `-` consumes an old line (shown),
/// `+` consumes a new line (shown), context consumes both (new shown).
fn numbered_diff_row(
    row: &str,
    state: &mut Option<(usize, usize)>,
) -> Option<(Tone, Option<usize>, String)> {
    if let Some((old_start, new_start)) = parse_hunk_header(row) {
        *state = Some((old_start, new_start));
        return None;
    }
    let (deletes, inserts, content) = match row.chars().next() {
        Some('-') => (true, false, &row[1..]),
        Some('+') => (false, true, &row[1..]),
        Some(' ') => (false, false, &row[1..]),
        _ => (false, false, row),
    };
    let counters = state.get_or_insert((1, 1));
    match (deletes, inserts) {
        // Deletion: consumes an old line, shown with its old number.
        (true, false) => {
            let old_number = counters.0;
            counters.0 += 1;
            Some((Tone::DiffDel, Some(old_number), truncate_content(content)))
        }
        // Insertion: consumes a new line, shown with its new number.
        (false, true) => {
            let new_number = counters.1;
            counters.1 += 1;
            Some((Tone::DiffIns, Some(new_number), truncate_content(content)))
        }
        // Context: consumes both sides; shows its new number.
        _ => {
            counters.0 += 1;
            let new_number = counters.1;
            counters.1 += 1;
            Some((Tone::Plain, Some(new_number), truncate_content(content)))
        }
    }
}

fn truncate_content(content: &str) -> String {
    text::truncate(content.trim_end(), BODY_COLS)
}

/// Parses `@@ -a[,b] +c[,d] @@` into `(a, c)`.
fn parse_hunk_header(row: &str) -> Option<(usize, usize)> {
    let rest = row.strip_prefix("@@ -")?;
    let (old_part, rest) = rest.split_once(' ')?;
    let new_part = rest.strip_prefix('+')?.trim_end();
    let new_part = new_part.strip_suffix("@@")?.trim_end();
    let old_start = old_part
        .split_once(',')
        .map_or(old_part, |(start, _)| start);
    let new_start = new_part
        .split_once(',')
        .map_or(new_part, |(start, _)| start);
    Some((old_start.parse().ok()?, new_start.parse().ok()?))
}

/// Bounded `output` / `locs` body: blank rows drop out, each row indents by
/// four and truncates horizontally.
fn indented_rows(source: &str, cap: usize) -> BodyRows {
    finish(
        source
            .lines()
            .filter(|row| !row.trim().is_empty())
            .map(|row| {
                card_line(
                    format!("    {}", text::truncate(row, BODY_COLS.saturating_sub(4))),
                    Tone::Plain,
                )
            })
            .collect(),
        cap,
    )
}

fn finish(all: Vec<TranscriptLine>, cap: usize) -> BodyRows {
    let extra = all.len().checked_sub(cap).filter(|extra| *extra > 0);
    BodyRows {
        lines: all.into_iter().take(cap).collect(),
        extra,
    }
}

pub(crate) fn verbose_card(
    tool_name: &str,
    index: usize,
    batch_size: usize,
    arguments: &str,
    result: &FrontendToolResult,
    display: Option<&FrontendToolDisplay>,
) -> Vec<TranscriptLine> {
    let total = batch_size.max(index + 1);
    let mut lines = vec![card_line(
        format!("{tool_name} {}/{}", index + 1, total),
        Tone::Title,
    )];
    if !arguments.trim().is_empty() {
        lines.push(card_line(
            format!("  args  {}", compact_args(arguments)),
            Tone::Detail,
        ));
    }
    match result {
        FrontendToolResult::Success { content } => {
            lines.push(card_line("  ok", Tone::Detail));
            lines.extend(
                content
                    .lines()
                    .map(|row| card_line(format!("  {row}"), Tone::Plain)),
            );
        }
        FrontendToolResult::Error { code, message } => {
            lines.push(card_line(format!("  error {code}"), Tone::Failure));
            lines.extend(
                message
                    .lines()
                    .map(|row| card_line(format!("  {row}"), Tone::Plain)),
            );
        }
    }
    if let Some(display) = display {
        if !display.detail.is_empty() {
            lines.push(card_line("  detail", Tone::Detail));
            lines.extend(
                display
                    .detail
                    .lines()
                    .map(|row| card_line(format!("    {row}"), Tone::Plain)),
            );
        }
        if !display.facts.is_empty() {
            let facts = display
                .facts
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("  ");
            lines.push(card_line(format!("  facts  {facts}"), Tone::Detail));
        }
    }
    lines
}

fn card_line(text: impl Into<String>, tone: Tone) -> TranscriptLine {
    TranscriptLine {
        kind: LineKind::Tool,
        text: text.into(),
        tone,
    }
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

fn fact<'a>(display: Option<&'a FrontendToolDisplay>, name: &str) -> Option<&'a str> {
    display?
        .facts
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

#[cfg(test)]
mod tests {
    use philo_agent_service::{FrontendToolDisplay, FrontendToolResult};

    use super::*;

    fn texts(lines: &[TranscriptLine]) -> Vec<&str> {
        lines.iter().map(|line| line.text.as_str()).collect()
    }

    fn tones(lines: &[TranscriptLine]) -> Vec<Tone> {
        lines.iter().map(|line| line.tone).collect()
    }

    fn display(detail: impl Into<String>, facts: &[(&str, &str)]) -> FrontendToolDisplay {
        FrontendToolDisplay {
            detail: detail.into(),
            facts: facts
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        }
    }

    fn success(content: impl Into<String>) -> FrontendToolResult {
        FrontendToolResult::Success {
            content: content.into(),
        }
    }

    fn error(code: &str, message: &str) -> FrontendToolResult {
        FrontendToolResult::Error {
            code: code.to_owned(),
            message: message.to_owned(),
        }
    }

    #[test]
    fn read_card_counts_files_and_lists_subjects() {
        let read = display(
            "",
            &[
                ("title", "Read"),
                ("verb", "Read"),
                ("body", "none"),
                ("subject", "src/routes/users.ts"),
                ("subject", "src/routes/users.test.ts"),
                ("count", "2 files"),
            ],
        );
        let lines = default_card(
            "read",
            r#"{"paths":["src/routes/users.ts"]}"#,
            &success("contents"),
            Some(&read),
        );
        assert_eq!(
            texts(&lines),
            [
                "Read 2 files",
                "  ↳ src/routes/users.ts",
                "    src/routes/users.test.ts"
            ]
        );
        assert_eq!(tones(&lines), [Tone::Title, Tone::Detail, Tone::Detail]);
        assert!(lines.iter().all(|line| !line.text.contains("contents")));
    }

    #[test]
    fn grep_card_keeps_pattern_detail_and_stays_silent_about_the_dump() {
        let locs = (1..=8)
            .map(|i| format!("src/lib.rs:{i}: hit {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let grep = display(
            locs,
            &[
                ("title", "Grep"),
                ("verb", "Searched"),
                ("body", "locs"),
                ("subject", "\"hit\""),
                ("count", "1 search"),
                ("matches_total", "8"),
            ],
        );
        let lines = default_card(
            "grep",
            r#"{"pattern":"hit","path":"src"}"#,
            &success("dump of every match for the model"),
            Some(&grep),
        );
        assert_eq!(
            texts(&lines),
            [
                "Grep 1 search",
                "  ↳ \"hit\"",
                "",
                "    src/lib.rs:1: hit 1",
                "    src/lib.rs:2: hit 2",
                "    src/lib.rs:3: hit 3",
                "    src/lib.rs:4: hit 4",
                "    src/lib.rs:5: hit 5",
            ]
        );
        assert!(tones(&lines)[3] == Tone::Plain);
        assert!(
            lines
                .iter()
                .all(|line| !line.text.contains("dump of every match"))
        );
        assert!(lines.iter().all(|line| !line.text.contains("hit 8")));
    }

    #[test]
    fn list_card_is_header_plus_directory_subject_only() {
        let list = display(
            "src/main.rs\nsrc/lib.rs",
            &[
                ("title", "List Directory"),
                ("verb", "Listed"),
                ("body", "none"),
                ("subject", "."),
                ("count", "1 directory"),
            ],
        );
        let lines = default_card(
            "list",
            r#"{"path":"."}"#,
            &success("src/main.rs"),
            Some(&list),
        );
        assert_eq!(texts(&lines), ["List Directory 1 directory", "  ↳ ."]);
        assert!(lines.iter().all(|line| !line.text.contains("src/main.rs")));
    }

    #[test]
    fn edit_card_renders_result_row_and_numbered_gutter() {
        let detail = "@@ -6,3 +6,3 @@\n-const limit = page * 10;\n+const limit = Math.min(page * 10, 50);\n+const offset = page * limit;\n return paginate(page);";
        let edit = display(
            detail,
            &[
                ("title", "Edit"),
                ("verb", "Edited"),
                ("body", "diff"),
                ("subject", "src/routes/users.ts"),
                ("result", "Succeeded. File edited.  (+2 added, -1 removed)"),
            ],
        );
        let lines = default_card(
            "edit",
            r#"{"path":"src/routes/users.ts"}"#,
            &success("replaced src/routes/users.ts"),
            Some(&edit),
        );
        assert_eq!(
            texts(&lines),
            [
                "Edit src/routes/users.ts",
                "  ↳ Succeeded. File edited.  (+2 added, -1 removed)",
                "",
                "    6 | const limit = page * 10;",
                "    6 | const limit = Math.min(page * 10, 50);",
                "    7 | const offset = page * limit;",
                "    8 | return paginate(page);",
            ]
        );
        assert_eq!(
            tones(&lines),
            [
                Tone::Title,
                Tone::Detail,
                Tone::Plain,
                Tone::DiffDel,
                Tone::DiffIns,
                Tone::DiffIns,
                Tone::Plain,
            ]
        );
        assert!(lines.iter().all(|line| !line.text.contains("@@")));
        assert!(lines.iter().all(|line| !line.text.contains("replaced src")));
    }

    #[test]
    fn write_cards_number_plus_lines_from_one_without_a_header() {
        let write = display(
            "+hello\n+world",
            &[
                ("title", "Write"),
                ("verb", "Added"),
                ("body", "diff"),
                ("subject", "src/a.rs"),
                ("result", "Succeeded. File created.  (+2 added)"),
            ],
        );
        let lines = default_card(
            "write",
            r#"{"path":"src/a.rs"}"#,
            &success("wrote src/a.rs (11 bytes, created)"),
            Some(&write),
        );
        assert_eq!(
            texts(&lines),
            [
                "Write src/a.rs",
                "  ↳ Succeeded. File created.  (+2 added)",
                "",
                "    1 | hello",
                "    2 | world",
            ]
        );
        assert_eq!(
            tones(&lines),
            [
                Tone::Title,
                Tone::Detail,
                Tone::Plain,
                Tone::DiffIns,
                Tone::DiffIns,
            ]
        );
    }

    #[test]
    fn run_card_reports_exit_and_duration_with_bounded_output() {
        let run = display(
            "ok\npassed",
            &[
                ("title", "Run"),
                ("verb", "Ran"),
                ("body", "output"),
                ("subject", "pnpm test"),
                ("count", "1 command"),
                ("result", "exit 0 · 4.2s"),
            ],
        );
        let lines = default_card(
            "shell",
            r#"{"command":"pnpm test"}"#,
            &success("exit_code: 0\nok"),
            Some(&run),
        );
        assert_eq!(
            texts(&lines),
            [
                "Run 1 command",
                "  ↳ pnpm test",
                "  ↳ exit 0 · 4.2s",
                "",
                "    ok",
                "    passed",
            ]
        );

        let failed = display(
            "boom",
            &[
                ("title", "Run"),
                ("verb", "Ran"),
                ("body", "output"),
                ("subject", "cargo test"),
                ("count", "1 command"),
                ("result", "exit 1 · 1.2s"),
            ],
        );
        let lines = default_card(
            "shell",
            r#"{"command":"cargo test"}"#,
            &success("exit_code: 1\nbloom"),
            Some(&failed),
        );
        assert_eq!(
            texts(&lines),
            [
                "Run 1 command",
                "  ↳ cargo test",
                "  ↳ exit 1 · 1.2s",
                "",
                "    boom",
            ]
        );
        assert!(lines.iter().all(|line| !line.text.contains("bloom")));
    }

    #[test]
    fn failure_keeps_the_header_shape_and_adds_a_red_row() {
        let lines = default_card(
            "edit",
            r#"{"path":"src/lib.rs"}"#,
            &error("not_unique", "3 matches"),
            None,
        );
        assert_eq!(
            texts(&lines),
            ["edit src/lib.rs", "  ↳ Failed. not_unique · 3 matches"]
        );
        assert_eq!(tones(&lines), [Tone::Title, Tone::Failure]);
    }

    #[test]
    fn output_body_caps_at_sixteen_lines_with_a_footer() {
        let detail = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let run = display(
            detail,
            &[
                ("title", "Run"),
                ("verb", "Ran"),
                ("body", "output"),
                ("subject", "seq"),
                ("count", "1 command"),
                ("result", "exit 0"),
                ("truncated", "true"),
            ],
        );
        let lines = default_card(
            "shell",
            r#"{"command":"seq"}"#,
            &success("model dump 20"),
            Some(&run),
        );
        assert_eq!(lines[0].text, "Run 1 command");
        assert_eq!(lines[4].text, "    line 1");
        assert_eq!(lines[19].text, "    line 16");
        assert_eq!(lines[20].text, "  ↳ … +4 lines");
        assert_eq!(lines.len(), 21);
        assert!(tones(&lines)[20] == Tone::Detail);
        assert!(lines.iter().all(|line| !line.text.contains("line 20")));
        assert!(lines.iter().all(|line| !line.text.contains("model dump")));
    }

    #[test]
    fn missing_body_fact_never_dumps_display_detail() {
        let read = display("read completed\nfull display detail", &[("bytes", "840")]);
        let lines = default_card(
            "read_file",
            r#"{"path":"src/main.rs"}"#,
            &success("fn main() {}"),
            Some(&read),
        );
        assert_eq!(texts(&lines), ["read_file src/main.rs"]);
        assert!(
            lines
                .iter()
                .all(|line| !line.text.contains("full display detail"))
        );
    }

    #[test]
    fn missing_title_falls_back_to_the_tool_name() {
        let read = display(
            "",
            &[
                ("body", "none"),
                ("subject", "src/main.rs"),
                ("count", "1 file"),
            ],
        );
        let lines = default_card(
            "read_file",
            r#"{"path":"src/main.rs"}"#,
            &success(""),
            Some(&read),
        );
        assert_eq!(texts(&lines), ["read_file 1 file", "  ↳ src/main.rs"]);
    }

    #[test]
    fn verbose_card_keeps_its_structure_with_new_tokens() {
        let verbose = display("read 12 bytes", &[("bytes", "12")]);
        let lines = verbose_card(
            "read_file",
            0,
            2,
            r#"{"path":"src/main.rs"}"#,
            &success("fn main() {}"),
            Some(&verbose),
        );
        assert_eq!(
            texts(&lines),
            [
                "read_file 1/2",
                "  args  path: src/main.rs",
                "  ok",
                "  fn main() {}",
                "  detail",
                "    read 12 bytes",
                "  facts  bytes=12",
            ]
        );
        assert_eq!(
            tones(&lines),
            [
                Tone::Title,
                Tone::Detail,
                Tone::Detail,
                Tone::Plain,
                Tone::Detail,
                Tone::Plain,
                Tone::Detail,
            ]
        );
    }
}
