//! Default-mode tool cards as a generic `FrontendToolDisplay` projection
//! (v4.0 P3): the unified `▎` header formula, card bodies, the live running
//! card, the concurrent tree, and the diff gutter.
//!
//! Cards are `Tool` cells. The header projects into one single-row cell
//! (`▎ [action] [target] [stats] ····· [status] [time]`); the body, when
//! present, is a foldable [`CardBody`] cell. The live running card and the
//! concurrent tree each ride one cell, rewritten in place as
//! started/progress/completed events land (the App owns that bookkeeping in
//! `super::live_tool`). The TUI holds zero tool knowledge — every piece
//! comes from the frozen facts vocabulary (`title` / repeatable `subject` /
//! `count` / `result` / `body`) supplied by tools-std.
//!
//! Verbose mode keeps its older structure (full args, model-facing result,
//! detail/facts) and only swaps tokens.

use std::time::Duration;

use philo_agent_service::{FrontendToolDisplay, FrontendToolResult};

use super::run_state::format_card_elapsed;
use super::text;
use super::transcript::{
    body_line, card_cell, header_line, CardBody, CardHeader, HeaderPiece, LineKind, SegColor,
    SegSpan, Tone, TranscriptLine, compact_args, preview,
};

const KEY_WIDTH: usize = 40;
const BODY_COLS: usize = 200;
/// v4.0 P3 §6: completion cards fold once the body passes this many rows.
pub(crate) const FOLD_THRESHOLD: usize = 8;
/// v4.0 P3 §4: the live output cap in characters; past it a truncated marker
/// replaces the tail.
pub(crate) const LIVE_TEXT_CHARS_MAX: usize = 1600;

/// One settled card: header cell, the remaining subject rows, and the
/// foldable body. `elapsed` is the settle duration from the App's slot
/// clock (TUI wall clock, §1); replay passes `None` and renders no time.
pub(crate) fn default_card(
    tool_name: &str,
    arguments: &str,
    result: &FrontendToolResult,
    display: Option<&FrontendToolDisplay>,
    elapsed: Option<Duration>,
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

    let mut lines = vec![card_header_cell(title, arguments, &subjects, display, result, elapsed)];
    if let FrontendToolResult::Error { code, message } = result {
        lines.push(card_line(
            format!("  Failed. {code} · {}", preview(message, 80)),
            Tone::Failure,
        ));
        return lines;
    }
    // Subjects after the first ride as plain indented rows (§2); the header
    // already carries the first subject (or the primary argument key).
    for subject in subjects.iter().skip(1) {
        lines.push(card_line(format!("  {subject}"), Tone::Detail));
    }
    push_body(&mut lines, display);
    lines
}

/// The card header cell, built from the facts (and, for the target, the
/// argument keys). Colors come from the family classification (§3):
/// edit/write families are distinguishable by their frozen facts
/// (`operation` / `bytes_before` / `exit_code`), so the TUI never guesses.
fn card_header_cell(
    title: &str,
    arguments: &str,
    subjects: &[String],
    display: Option<&FrontendToolDisplay>,
    result: &FrontendToolResult,
    elapsed: Option<Duration>,
) -> TranscriptLine {
    let (bar, status, bold) = state_for(result, display);
    let target = match subjects.first() {
        Some(first) => Some(HeaderPiece {
            text: preview(first, KEY_WIDTH),
            color: target_color(arguments),
            bold: false,
        }),
        None => primary_target(arguments).map(|(target, kind)| HeaderPiece {
            text: preview(&target, KEY_WIDTH),
            color: kind_color(kind),
            bold: false,
        }),
    };
    header_line(CardHeader {
        bar: HeaderPiece {
            text: "▎".to_owned(),
            color: bar,
            bold: false,
        },
        action: HeaderPiece {
            text: title.to_owned(),
            color: SegColor::Gray,
            bold: true,
        },
        target,
        stats: stats_segments(display),
        status: HeaderPiece {
            text: status.to_owned(),
            color: bar,
            bold,
        },
        time: elapsed.map(|elapsed| HeaderPiece {
            text: format_card_elapsed(elapsed),
            color: SegColor::DarkGray,
            bold: false,
        }),
    })
}

/// The state classification: bar color, status word, and status weight.
pub(crate) fn state_for(
    result: &FrontendToolResult,
    display: Option<&FrontendToolDisplay>,
) -> (SegColor, &'static str, bool) {
    match result {
        FrontendToolResult::Error { .. } => (SegColor::Red, "✗ failed", false),
        FrontendToolResult::Success { .. } => {
            if fact(display, "operation").is_some() {
                (SegColor::Green, "✓ created", false)
            } else if fact(display, "bytes_before").is_some() {
                (SegColor::Orange, "✓ applied", false)
            } else if let Some(code) = fact(display, "exit_code") {
                if code == "0" {
                    (SegColor::Green, "✓ done", false)
                } else {
                    (SegColor::Red, "✗ failed", false)
                }
            } else {
                (SegColor::Green, "✓ done", false)
            }
        }
    }
}

/// The stats cluster: `(+42 lines)` green (reads), `(+2 -1)` green/red
/// (edits), `(+N lines)` green (writes), `N matches`/`N entries` gray
/// (grep/list), nothing for runs. Facts only.
fn stats_segments(display: Option<&FrontendToolDisplay>) -> Option<Vec<SegSpan>> {
    match fact(display, "body") {
        Some("diff") if fact(display, "bytes_before").is_some() => {
            let mut segs = Vec::new();
            if let Some(added) = fact(display, "added") {
                segs.push(SegSpan::colored(format!("(+{added}"), SegColor::Green));
            }
            if let Some(removed) = fact(display, "removed") {
                segs.push(SegSpan::colored(format!(" -{removed})"), SegColor::Red));
            }
            (!segs.is_empty()).then_some(segs)
        }
        Some("diff") => fact(display, "added").map(|added| {
            vec![SegSpan::colored(format!("(+{added} lines)"), SegColor::Green)]
        }),
        Some("locs") => {
            if let Some(total) = fact(display, "matches_total") {
                Some(vec![SegSpan::colored(format!("{total} matches"), SegColor::Gray)])
            } else {
                count_segment(display)
            }
        }
        _ => {
            if let Some(total) = fact(display, "lines_total") {
                Some(vec![SegSpan::colored(format!("(+{total} lines)"), SegColor::Green)])
            } else if let Some(entries) = fact(display, "entries_total") {
                Some(vec![SegSpan::colored(format!("{entries} entries"), SegColor::Gray)])
            } else {
                count_segment(display)
            }
        }
    }
}

/// The frozen `count` fact (`2 files`, `1 directory`) as gray stats.
fn count_segment(display: Option<&FrontendToolDisplay>) -> Option<Vec<SegSpan>> {
    fact(display, "count").map(|count| vec![SegSpan::colored(count, SegColor::Gray)])
}

/// The foldable body cell (output / locs / diff). Bodies over the threshold
/// fold by default (§6); the App's fold state can open or close one.
fn push_body(lines: &mut Vec<TranscriptLine>, display: Option<&FrontendToolDisplay>) {
    let Some(display) = display else {
        return;
    };
    let segments = match fact(Some(display), "body") {
        Some("diff") => diff_segments(&display.detail),
        Some("output") | Some("locs") => indented_segments(&display.detail),
        _ => return,
    };
    if segments.is_empty() {
        return;
    }
    let fold_count = segments.len().saturating_sub(3);
    lines.push(body_line(CardBody {
        lines: segments,
        threshold: FOLD_THRESHOLD,
        fold_default_collapsed: true,
        fold_count,
        fold_label: "行已折叠".to_owned(),
        fold_hint: true,
        fold_all: false,
    }));
}

/// Diff body as segmented rows: a fixed 4-column number slot (`-  3`,
/// `+  3`, `   2`), the BORDER `│` separator, then the content. Del/ins
/// rows carry their wash tone; the hunk header never renders.
fn diff_segments(source: &str) -> Vec<Vec<SegSpan>> {
    let mut state: Option<(usize, usize)> = None;
    let mut lines: Vec<Vec<SegSpan>> = Vec::new();
    for row in source.lines() {
        if let Some((old_start, new_start)) = parse_hunk_header(row) {
            state = Some((old_start, new_start));
            continue;
        }
        let (tone, number, content) = numbered_diff_row(row, &mut state);
        let number = number.unwrap_or(0);
        let (symbol, number_color, content_color, wash) = match tone {
            Tone::DiffDel => ("-", SegColor::Red, SegColor::Red, Some(Tone::DiffDel)),
            Tone::DiffIns => ("+", SegColor::Green, SegColor::Green, Some(Tone::DiffIns)),
            _ => (" ", SegColor::DarkGray, SegColor::Default, None),
        };
        let number_text = match tone {
            Tone::DiffDel | Tone::DiffIns => format!("{symbol}  {number}"),
            _ => format!("   {number}"),
        };
        lines.push(vec![
            SegSpan {
                text: number_text,
                color: number_color,
                bold: false,
                tone: wash,
            },
            SegSpan {
                text: "│ ".to_owned(),
                color: SegColor::Border,
                bold: false,
                tone: None,
            },
            SegSpan {
                text: content,
                color: content_color,
                bold: false,
                tone: None,
            },
        ]);
    }
    lines
}

/// Classifies one hunk row against the rolling `(old_next, new_next)`
/// counters seeded by the hunk header: `-` consumes an old line (shown),
/// `+` consumes a new line (shown), context consumes both (new shown).
fn numbered_diff_row(
    row: &str,
    state: &mut Option<(usize, usize)>,
) -> (Tone, Option<usize>, String) {
    if let Some((old_start, new_start)) = parse_hunk_header(row) {
        *state = Some((old_start, new_start));
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
            (Tone::DiffDel, Some(old_number), truncate_content(content))
        }
        // Insertion: consumes a new line, shown with its new number.
        (false, true) => {
            let new_number = counters.1;
            counters.1 += 1;
            (Tone::DiffIns, Some(new_number), truncate_content(content))
        }
        // Context: consumes both sides; shows its new number.
        _ => {
            counters.0 += 1;
            let new_number = counters.1;
            counters.1 += 1;
            (Tone::Plain, Some(new_number), truncate_content(content))
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

/// Bounded `output` / `locs` body: blank rows drop out, each row truncates
/// horizontally. The body indent (2 columns) is applied at projection.
fn indented_segments(source: &str) -> Vec<Vec<SegSpan>> {
    source
        .lines()
        .filter(|row| !row.trim().is_empty())
        .map(|row| vec![SegSpan::plain(text::truncate(row, BODY_COLS))])
        .collect()
}

/// The live running card, one cell (§4): header (yellow bar, spinner,
/// elapsed) plus the bounded live output body.
pub(crate) fn running_cell(
    tool_name: &str,
    arguments: &str,
    output: &str,
    truncated: bool,
    spinner: &str,
    elapsed: Duration,
) -> TranscriptLine {
    let target = primary_target(arguments).map(|(target, kind)| HeaderPiece {
        text: preview(&target, KEY_WIDTH),
        color: kind_color(kind),
        bold: false,
    });
    let header = CardHeader {
        bar: HeaderPiece {
            text: "▎".to_owned(),
            color: SegColor::Yellow,
            bold: false,
        },
        action: HeaderPiece {
            text: tool_name.to_owned(),
            color: SegColor::Gray,
            bold: true,
        },
        target,
        stats: None,
        status: HeaderPiece {
            text: spinner.to_owned(),
            color: SegColor::Yellow,
            bold: false,
        },
        time: Some(HeaderPiece {
            text: format_card_elapsed(elapsed),
            color: SegColor::DarkGray,
            bold: false,
        }),
    };
    let mut lines = indented_segments(output);
    if truncated {
        lines.push(vec![SegSpan::colored("… (truncated)", SegColor::Gray)]);
    }
    card_cell(
        header,
        CardBody {
            lines,
            threshold: usize::MAX,
            fold_default_collapsed: false,
            fold_count: 0,
            fold_label: "行已折叠".to_owned(),
            fold_hint: true,
            fold_all: false,
        },
    )
}

/// A cancelled card: red `✗ cancelled` header, no body (§2 priority).
pub(crate) fn cancelled_cell(tool_name: &str, arguments: &str) -> TranscriptLine {
    let target = primary_target(arguments).map(|(target, kind)| HeaderPiece {
        text: preview(&target, KEY_WIDTH),
        color: kind_color(kind),
        bold: false,
    });
    header_line(CardHeader {
        bar: HeaderPiece {
            text: "▎".to_owned(),
            color: SegColor::Red,
            bold: false,
        },
        action: HeaderPiece {
            text: tool_name.to_owned(),
            color: SegColor::Gray,
            bold: true,
        },
        target,
        stats: None,
        status: HeaderPiece {
            text: "✗ cancelled".to_owned(),
            color: SegColor::Red,
            bold: false,
        },
        time: None,
    })
}

/// The primary argument key with its kind (path / command / pattern) and
/// the color that kind paints as.
pub(crate) fn preview_target(arguments: &str) -> Option<(String, SegColor)> {
    primary_target(arguments).map(|(target, kind)| (preview(&target, KEY_WIDTH), kind_color(kind)))
}

/// Target kind of the arguments JSON, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArgKind {
    Path,
    Command,
    Pattern,
}

fn arg_kind(arguments: &str) -> Option<ArgKind> {
    for key in ["command", "pattern", "paths", "path"] {
        if json_has_field(arguments, key) {
            return Some(match key {
                "command" => ArgKind::Command,
                "pattern" => ArgKind::Pattern,
                _ => ArgKind::Path,
            });
        }
    }
    None
}

fn kind_color(kind: ArgKind) -> SegColor {
    match kind {
        ArgKind::Path => SegColor::Green,
        ArgKind::Command | ArgKind::Pattern => SegColor::Orange,
    }
}

fn target_color(arguments: &str) -> SegColor {
    arg_kind(arguments).map(kind_color).unwrap_or(SegColor::Green)
}

fn json_has_field(raw: &str, key: &str) -> bool {
    raw.contains(&format!("\"{key}\""))
}

/// First string value under the primary keys, accepting arrays (`paths`).
fn primary_target(arguments: &str) -> Option<(String, ArgKind)> {
    if let Some(value) = json_string_field(arguments, "path")
        .or_else(|| json_string_field(arguments, "paths"))
    {
        return Some((value, ArgKind::Path));
    }
    if let Some(value) = json_string_field(arguments, "command") {
        return Some((value, ArgKind::Command));
    }
    if let Some(value) = json_string_field(arguments, "pattern") {
        return Some((value, ArgKind::Pattern));
    }
    None
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
        header: None,
        body: None,
    }
}

fn json_string_field(raw: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = raw.find(&needle)?;
    let after = raw[start + needle.len()..].trim_start().strip_prefix(':')?;
    let after = after.trim_start();
    // Accept arrays (`paths: [...]`) by reading the first element.
    let after = if let Some(rest) = after.strip_prefix('[') {
        rest.trim_start()
    } else {
        after
    };
    let after = after.strip_prefix('"')?;
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

    fn card_header(lines: &[TranscriptLine]) -> &CardHeader {
        lines[0].header.as_ref().expect("header cell")
    }

    fn body(lines: &[TranscriptLine]) -> &CardBody {
        lines[1].body.as_ref().expect("body cell")
    }

    fn stats(header: &CardHeader) -> Option<Vec<String>> {
        header
            .stats
            .as_ref()
            .map(|segs| segs.iter().map(|seg| seg.text.clone()).collect())
    }

    fn row_texts(rows: &[Vec<SegSpan>]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                row.iter()
                    .map(|seg| seg.text.as_str())
                    .collect::<Vec<_>>()
                    .concat()
            })
            .collect()
    }

    fn wash(rows: &[Vec<SegSpan>]) -> Vec<Option<Tone>> {
        rows.iter()
            .map(|row| row.first().and_then(|seg| seg.tone))
            .collect()
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
            None,
        );
        let header = card_header(&lines);
        assert_eq!(header.action.text, "Read");
        assert!(header.action.bold);
        let target = header.target.as_ref().expect("first subject is the target");
        assert_eq!(target.text, "src/routes/users.ts");
        assert_eq!(target.color, SegColor::Green);
        assert_eq!(stats(header), Some(vec!["2 files".to_owned()]));
        assert_eq!(header.status.text, "✓ done");
        assert_eq!(header.status.color, SegColor::Green);
        assert_eq!(header.time, None);
        // The remaining subjects ride as plain indented rows.
        assert_eq!(texts(&lines), ["", "  src/routes/users.test.ts"]);
        assert_eq!(lines[1].tone, Tone::Detail);
        assert_eq!(tones(&lines), [Tone::Title, Tone::Detail]);
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
            None,
        );
        let header = card_header(&lines);
        assert_eq!(header.action.text, "Grep");
        let target = header.target.as_ref().expect("pattern subject");
        assert_eq!(target.text, "\"hit\"");
        assert_eq!(target.color, SegColor::Orange);
        assert_eq!(stats(header), Some(vec!["8 matches".to_owned()]));
        assert_eq!(header.status.text, "✓ done");
        let rows = &body(&lines).lines;
        assert_eq!(rows.len(), 8);
        assert_eq!(rows[0][0].text, "src/lib.rs:1: hit 1");
        assert_eq!(rows[7][0].text, "src/lib.rs:8: hit 8");
        assert!(
            lines
                .iter()
                .all(|line| !line.text.contains("dump of every match"))
        );
    }

    #[test]
    fn list_card_is_header_plus_directory_subject_only() {
        let list = display(
            "src/main.rs\nsrc/lib.rs",
            &[
                ("title", "List Directory"),
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
            None,
        );
        let header = card_header(&lines);
        assert_eq!(header.action.text, "List Directory");
        assert_eq!(
            header.target.as_ref().map(|t| t.text.as_str()),
            Some(".")
        );
        assert_eq!(header.target.as_ref().map(|t| t.color), Some(SegColor::Green));
        assert_eq!(stats(header), Some(vec!["1 directory".to_owned()]));
        assert_eq!(lines.len(), 1, "no body for a directory listing");
        assert!(lines.iter().all(|line| !line.text.contains("src/main.rs")));
    }

    #[test]
    fn edit_card_renders_the_result_row_and_numbered_gutter() {
        let detail = "@@ -6,3 +6,3 @@\n-const limit = page * 10;\n+const limit = Math.min(page * 10, 50);\n+const offset = page * limit;\n return paginate(page);";
        let edit = display(
            detail,
            &[
                ("title", "Edit"),
                ("body", "diff"),
                ("subject", "src/routes/users.ts"),
                ("added", "2"),
                ("removed", "1"),
                ("bytes_before", "840"),
                ("result", "Succeeded. File edited.  (+2 added, -1 removed)"),
            ],
        );
        let lines = default_card(
            "edit",
            r#"{"path":"src/routes/users.ts"}"#,
            &success("replaced src/routes/users.ts"),
            Some(&edit),
            None,
        );
        let header = card_header(&lines);
        // Edits are their own family: orange bar, `✓ applied`, green path.
        assert_eq!(header.bar.color, SegColor::Orange);
        assert_eq!(header.status.text, "✓ applied");
        assert_eq!(header.status.color, SegColor::Orange);
        assert_eq!(
            header.target.as_ref().map(|t| t.color),
            Some(SegColor::Green)
        );
        let stats = header.stats.as_ref().expect("edit stats");
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].text, "(+2");
        assert_eq!(stats[0].color, SegColor::Green);
        assert_eq!(stats[1].text, " -1)");
        assert_eq!(stats[1].color, SegColor::Red);
        // The gutter is 4 columns wide; the hunk header never renders.
        let rows = &body(&lines).lines;
        assert_eq!(
            row_texts(rows),
            [
                "-  6│ const limit = page * 10;",
                "+  6│ const limit = Math.min(page * 10, 50);",
                "+  7│ const offset = page * limit;",
                "   8│ return paginate(page);",
            ]
        );
        assert_eq!(wash(rows), [
            Some(Tone::DiffDel),
            Some(Tone::DiffIns),
            Some(Tone::DiffIns),
            None,
        ]);
        assert!(lines.iter().all(|line| !line.text.contains("@@")));
        assert!(lines.iter().all(|line| !line.text.contains("replaced src")));
    }

    #[test]
    fn write_cards_number_plus_lines_from_one() {
        let write = display(
            "+hello\n+world",
            &[
                ("title", "Write"),
                ("body", "diff"),
                ("subject", "src/a.rs"),
                ("added", "2"),
                ("operation", "write"),
                ("result", "Succeeded. File created.  (+2 added)"),
            ],
        );
        let lines = default_card(
            "write",
            r#"{"path":"src/a.rs"}"#,
            &success("wrote src/a.rs (11 bytes, created)"),
            Some(&write),
            None,
        );
        let header = card_header(&lines);
        assert_eq!(header.bar.color, SegColor::Green);
        assert_eq!(header.status.text, "✓ created");
        assert_eq!(header.status.color, SegColor::Green);
        assert_eq!(stats(header), Some(vec!["(+2 lines)".to_owned()]));
        let rows = &body(&lines).lines;
        assert_eq!(row_texts(rows), ["+  1│ hello", "+  2│ world"]);
        assert_eq!(wash(rows), [Some(Tone::DiffIns), Some(Tone::DiffIns)]);
        assert_eq!(tones(&lines), [Tone::Title, Tone::Plain]);
    }

    #[test]
    fn run_card_reports_exit_and_duration_with_bounded_output() {
        let run = display(
            "ok\npassed",
            &[
                ("title", "Run"),
                ("body", "output"),
                ("subject", "pnpm test"),
                ("count", "1 command"),
                ("exit_code", "0"),
                ("result", "exit 0 · 4.2s"),
            ],
        );
        let lines = default_card(
            "shell",
            r#"{"command":"pnpm test"}"#,
            &success("exit_code: 0\nok"),
            Some(&run),
            None,
        );
        let header = card_header(&lines);
        // Runs paint their command target orange, no stats beyond the count.
        assert_eq!(
            header.target.as_ref().map(|t| t.color),
            Some(SegColor::Orange)
        );
        assert_eq!(stats(header), Some(vec!["1 command".to_owned()]));
        assert_eq!(header.status.text, "✓ done");
        assert_eq!(header.status.color, SegColor::Green);
        assert_eq!(
            row_texts(&body(&lines).lines),
            ["ok", "passed"]
        );

        let failed = display(
            "boom",
            &[
                ("title", "Run"),
                ("body", "output"),
                ("subject", "cargo test"),
                ("count", "1 command"),
                ("exit_code", "1"),
                ("result", "exit 1 · 1.2s"),
            ],
        );
        let lines = default_card(
            "shell",
            r#"{"command":"cargo test"}"#,
            &success("exit_code: 1\nbloom"),
            Some(&failed),
            None,
        );
        let head = card_header(&lines);
        assert_eq!(head.bar.color, SegColor::Red);
        assert_eq!(head.status.text, "✗ failed");
        assert_eq!(head.status.color, SegColor::Red);
        assert!(lines.iter().all(|line| !line.text.contains("bloom")));
    }

    #[test]
    fn failure_keeps_the_header_shape_and_adds_a_red_row() {
        let lines = default_card(
            "edit",
            r#"{"path":"src/lib.rs"}"#,
            &error("not_unique", "3 matches"),
            None,
            None,
        );
        let header = card_header(&lines);
        assert_eq!(header.bar.color, SegColor::Red);
        assert_eq!(header.action.text, "edit");
        assert_eq!(
            header.target.as_ref().map(|t| t.text.as_str()),
            Some("src/lib.rs")
        );
        assert_eq!(header.status.text, "✗ failed");
        assert_eq!(header.status.color, SegColor::Red);
        assert_eq!(
            texts(&lines),
            ["", "  Failed. not_unique · 3 matches"]
        );
        assert_eq!(tones(&lines), [Tone::Title, Tone::Failure]);
    }

    #[test]
    fn output_bodies_fold_past_the_threshold_with_a_counted_bar() {
        let detail = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let run = display(
            detail,
            &[
                ("title", "Run"),
                ("body", "output"),
                ("subject", "seq"),
                ("count", "1 command"),
                ("exit_code", "0"),
            ],
        );
        let lines = default_card(
            "shell",
            r#"{"command":"seq"}"#,
            &success("model dump 20"),
            Some(&run),
            None,
        );
        let body = body(&lines);
        // All rows live in the cell; the projection folds past the threshold.
        assert_eq!(body.lines.len(), 20);
        assert_eq!(body.threshold, FOLD_THRESHOLD);
        assert!(body.fold_default_collapsed);
        assert_eq!(body.fold_count, 17);
        assert_eq!(body.fold_label, "行已折叠");
        assert!(body.fold_hint);
        assert!(!body.fold_all);
        assert_eq!(body.lines[0][0].text, "line 1");
        assert_eq!(body.lines[19][0].text, "line 20");
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
            None,
        );
        let header = card_header(&lines);
        assert_eq!(header.action.text, "read_file");
        assert_eq!(
            header.target.as_ref().map(|t| t.text.as_str()),
            Some("src/main.rs")
        );
        assert_eq!(stats(header), None);
        assert_eq!(lines.len(), 1, "no body without a body fact");
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
            None,
        );
        let header = card_header(&lines);
        assert_eq!(header.action.text, "read_file");
        assert_eq!(
            header.target.as_ref().map(|t| t.text.as_str()),
            Some("src/main.rs")
        );
        assert_eq!(stats(header), Some(vec!["1 file".to_owned()]));
        assert_eq!(lines.len(), 1);
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
