//! Session-replay display derivation (M10+).
//!
//! [`derive_display_for_replay`] reconstructs a transient display-channel
//! payload from the durable model-channel facts a session store already
//! keeps: a tool's `name`, its raw `arguments`, and its [`ToolResultOutcome`].
//! The output is structurally identical to the live `ToolDisplay` the same
//! tool emits on [`philo_tools::RichToolResult`] so the TUI can route replay
//! through the very same `default_card` projection it uses for live cards.
//!
//! The function is a pure projection — no filesystem access, no clock, no
//! I/O. Inputs come exclusively from the session store's model channel;
//! nothing here persists the display channel (the dual-channel contract in
//! `philo_tools::result` forbids that). Fields that the persisted facts
//! cannot recover (file totals, elapsed time, edit byte sizes) are simply
//! omitted; the TUI card renderer leaves their slots empty rather than
//! guessing.
//!
//! The six standard tools each get one static dispatch rule keyed off the
//! tool name constant. Unknown tool names return `None` and the TUI falls
//! back to a status-only card.

use philo_session::ToolResultOutcome;
use philo_tools::ToolDisplay;

use crate::args::optional_string;
use crate::display::{CardFacts, card, plus_lines};
use crate::edit::EDIT_TOOL_NAME;
use crate::grep::GREP_TOOL_NAME;
use crate::list::LIST_TOOL_NAME;
use crate::read::READ_TOOL_NAME;
use crate::shell::SHELL_TOOL_NAME;
use crate::write::WRITE_TOOL_NAME;

/// Derives a replay display projection from durable model-channel facts.
///
/// Inputs all come from the session store's model channel (already
/// persisted). The output is structurally identical to the live
/// [`ToolDisplay`] the same tool emits, so the TUI replay path routes
/// through the exact same `default_card` projection live cards use.
///
/// Each of the six standard tools dispatches on its name constant to one
/// static rule. Unknown tool names return `None`; the TUI degrades to a
/// status-only card (no stats, no body). Cancelled / Interrupted outcomes
/// carry no display and also return `None` — the TUI renders the `✗` red
/// state directly from the outcome.
pub fn derive_display_for_replay(
    tool_name: &str,
    arguments: &str,
    outcome: &ToolResultOutcome,
) -> Option<ToolDisplay> {
    let content = match outcome {
        ToolResultOutcome::Success { content } => content.as_str(),
        ToolResultOutcome::Error { .. } | ToolResultOutcome::Cancelled | ToolResultOutcome::Interrupted => {
            return None;
        }
    };
    match tool_name {
        READ_TOOL_NAME => Some(derive_read(arguments, content)),
        GREP_TOOL_NAME => Some(derive_grep(arguments, content)),
        LIST_TOOL_NAME => Some(derive_list(arguments, content)),
        WRITE_TOOL_NAME => Some(derive_write(arguments, content)),
        EDIT_TOOL_NAME => Some(derive_edit(arguments, content)),
        SHELL_TOOL_NAME => Some(derive_shell(arguments, content)),
        _ => None,
    }
}

fn derive_read(arguments: &str, content: &str) -> ToolDisplay {
    let path = optional_string(arguments, "path")
        .ok()
        .flatten()
        .unwrap_or_default();
    let lines_shown = content.lines().count();
    card("Read", "Read", "none", "")
        .subject(&path)
        .count("1 file")
        .with_fact("lines_shown", lines_shown.to_string())
}

fn derive_grep(arguments: &str, content: &str) -> ToolDisplay {
    let pattern = optional_string(arguments, "pattern")
        .ok()
        .flatten()
        .unwrap_or_default();
    let matches_total = content.lines().filter(|line| is_loc_line(line)).count();
    let detail = if content.is_empty() { String::new() } else { content.to_owned() };
    card("Grep", "Searched", "locs", detail)
        .subject(format!("\"{pattern}\""))
        .count("1 search")
        .with_fact("matches_total", matches_total.to_string())
}

fn derive_list(arguments: &str, content: &str) -> ToolDisplay {
    let path = optional_string(arguments, "path")
        .ok()
        .flatten()
        .unwrap_or_else(|| ".".to_owned());
    let entries_total = content.lines().count();
    card("List Directory", "Listed", "none", "")
        .subject(&path)
        .count("1 directory")
        .with_fact("entries_total", entries_total.to_string())
}

fn derive_write(arguments: &str, _content: &str) -> ToolDisplay {
    let path = optional_string(arguments, "path")
        .ok()
        .flatten()
        .unwrap_or_default();
    let file_content = optional_string(arguments, "content")
        .ok()
        .flatten()
        .unwrap_or_default();
    let (detail, added, _truncated) = plus_lines(&file_content, usize::MAX);
    card("Write", "Wrote", "diff", detail)
        .subject(&path)
        .with_fact("added", added.to_string())
        .with_fact("removed", "0")
}

fn derive_edit(arguments: &str, _content: &str) -> ToolDisplay {
    let path = optional_string(arguments, "path")
        .ok()
        .flatten()
        .unwrap_or_default();
    let old_string = optional_string(arguments, "old_string")
        .ok()
        .flatten()
        .unwrap_or_default();
    let new_string = optional_string(arguments, "new_string")
        .ok()
        .flatten()
        .unwrap_or_default();
    let removed = old_string.lines().count();
    let added = new_string.lines().count();
    let hunk = degraded_edit_hunk(&old_string, &new_string);
    card("Edit", "Edited", "diff", hunk)
        .subject(&path)
        .with_fact("added", added.to_string())
        .with_fact("removed", removed.to_string())
}

fn derive_shell(arguments: &str, content: &str) -> ToolDisplay {
    let command = optional_string(arguments, "command")
        .ok()
        .flatten()
        .unwrap_or_default();
    let detail = content.to_owned();
    card("Run", "Ran", "output", detail)
        .subject(&command)
        .count("1 command")
}

/// A grep loc line is `path:line:...` — at least two colons separate the
/// three fields. Lines without that shape (truncation notices, blank rows)
/// do not count as matches.
fn is_loc_line(line: &str) -> bool {
    let mut colons = line.match_indices(':');
    let Some(first) = colons.next() else {
        return false;
    };
    first.0 > 0 && colons.next().is_some()
}

/// Replay cannot reconstruct the `@@ -a,b +a,c @@` header or the context
/// lines (the original file text is not persisted), so the degraded hunk is
/// just the bare `-old` / `+new` blocks. The TUI diff gutter numbers them
/// starting from 1 since no hunk header seeds the counters.
fn degraded_edit_hunk(old_string: &str, new_string: &str) -> String {
    let mut rows = Vec::new();
    for line in old_string.lines() {
        rows.push(format!("-{line}"));
    }
    for line in new_string.lines() {
        rows.push(format!("+{line}"));
    }
    rows.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn success(content: &str) -> ToolResultOutcome {
        ToolResultOutcome::Success {
            content: content.to_owned(),
        }
    }

    fn error(code: &str, message: &str) -> ToolResultOutcome {
        ToolResultOutcome::Error {
            code: code.to_owned(),
            message: message.to_owned(),
        }
    }

    fn fact_names(display: &ToolDisplay) -> Vec<&str> {
        display.facts().iter().map(|fact| fact.name()).collect()
    }

    fn fact_value<'a>(display: &'a ToolDisplay, name: &str) -> Option<&'a str> {
        display
            .facts()
            .iter()
            .find(|fact| fact.name() == name)
            .map(|fact| fact.value())
    }

    #[test]
    fn read_derives_subject_count_and_lines_shown() {
        let args = r#"{"path":"src/main.rs"}"#;
        let content = "    1|fn main() {}\n    2|// trailing";
        let display = derive_display_for_replay(READ_TOOL_NAME, args, &success(content))
            .expect("read derives a display");
        assert_eq!(display.detail(), "");
        assert_eq!(fact_value(&display, "title"), Some("Read"));
        assert_eq!(fact_value(&display, "body"), Some("none"));
        assert_eq!(fact_value(&display, "subject"), Some("src/main.rs"));
        assert_eq!(fact_value(&display, "count"), Some("1 file"));
        assert_eq!(fact_value(&display, "lines_shown"), Some("2"));
        // lines_total / bytes_total are not recoverable — omitted.
        assert_eq!(fact_value(&display, "lines_total"), None);
        assert_eq!(fact_value(&display, "bytes_total"), None);
    }

    #[test]
    fn grep_derives_pattern_matches_and_locs_detail() {
        let args = r#"{"pattern":"hit"}"#;
        let content = "src/lib.rs:1: hit one\nsrc/lib.rs:8: hit two\n[grep truncated: limit]";
        let display = derive_display_for_replay(GREP_TOOL_NAME, args, &success(content))
            .expect("grep derives a display");
        assert_eq!(fact_value(&display, "title"), Some("Grep"));
        assert_eq!(fact_value(&display, "body"), Some("locs"));
        assert_eq!(fact_value(&display, "subject"), Some("\"hit\""));
        assert_eq!(fact_value(&display, "count"), Some("1 search"));
        // Only the two `path:line:` rows count; the notice row is ignored.
        assert_eq!(fact_value(&display, "matches_total"), Some("2"));
        // The detail is the model content verbatim.
        assert_eq!(display.detail(), content);
    }

    #[test]
    fn list_derives_entries_total_from_content_rows() {
        let args = r#"{"path":"."}"#;
        let content = "dir\tsrc\nfile\tmain.rs\n[list truncated: 2 of 50]";
        let display = derive_display_for_replay(LIST_TOOL_NAME, args, &success(content))
            .expect("list derives a display");
        assert_eq!(fact_value(&display, "title"), Some("List Directory"));
        assert_eq!(fact_value(&display, "subject"), Some("."));
        assert_eq!(fact_value(&display, "count"), Some("1 directory"));
        // Every line counts, including the truncation marker.
        assert_eq!(fact_value(&display, "entries_total"), Some("3"));
    }

    #[test]
    fn write_derives_added_plus_lines_without_operation_fact() {
        let args = r#"{"path":"src/a.rs","content":"hello\nworld\n"}"#;
        let content = "created src/a.rs (11 bytes)";
        let display = derive_display_for_replay(WRITE_TOOL_NAME, args, &success(content))
            .expect("write derives a display");
        assert_eq!(fact_value(&display, "title"), Some("Write"));
        assert_eq!(fact_value(&display, "body"), Some("diff"));
        assert_eq!(fact_value(&display, "subject"), Some("src/a.rs"));
        assert_eq!(fact_value(&display, "added"), Some("2"));
        assert_eq!(fact_value(&display, "removed"), Some("0"));
        // operation is not recoverable — omitted (TUI shows ✓ done, not ✓ created).
        assert_eq!(fact_value(&display, "operation"), None);
        assert_eq!(display.detail(), "+hello\n+world");
    }

    #[test]
    fn edit_derives_degraded_hunk_without_hunk_header_or_context() {
        let args = r#"{"path":"src/lib.rs","old_string":"old line","new_string":"new line"}"#;
        let content = "edited src/lib.rs: replaced 1 occurrence (10 -> 12 bytes)";
        let display = derive_display_for_replay(EDIT_TOOL_NAME, args, &success(content))
            .expect("edit derives a display");
        assert_eq!(fact_value(&display, "title"), Some("Edit"));
        assert_eq!(fact_value(&display, "body"), Some("diff"));
        assert_eq!(fact_value(&display, "subject"), Some("src/lib.rs"));
        assert_eq!(fact_value(&display, "added"), Some("1"));
        assert_eq!(fact_value(&display, "removed"), Some("1"));
        // bytes_before / bytes_after are not recoverable — omitted.
        assert_eq!(fact_value(&display, "bytes_before"), None);
        // No @@ header, no context rows — just the bare -old / +new block.
        assert_eq!(display.detail(), "-old line\n+new line");
        assert!(!display.detail().contains("@@"));
    }

    #[test]
    fn shell_derives_command_subject_and_output_detail() {
        let args = r#"{"command":"pnpm test"}"#;
        let content = "exit_code: 0\nok\npassed";
        let display = derive_display_for_replay(SHELL_TOOL_NAME, args, &success(content))
            .expect("shell derives a display");
        assert_eq!(fact_value(&display, "title"), Some("Run"));
        assert_eq!(fact_value(&display, "body"), Some("output"));
        assert_eq!(fact_value(&display, "subject"), Some("pnpm test"));
        assert_eq!(fact_value(&display, "count"), Some("1 command"));
        // exit_code / duration_ms are not recoverable from Success — omitted.
        assert_eq!(fact_value(&display, "exit_code"), None);
        assert_eq!(fact_value(&display, "duration_ms"), None);
        assert_eq!(display.detail(), content);
    }

    #[test]
    fn unknown_tool_name_returns_none() {
        let args = r#"{"path":"src/main.rs"}"#;
        assert!(derive_display_for_replay("read_file", args, &success("ok")).is_none());
        assert!(derive_display_for_replay("mystery_tool", args, &success("ok")).is_none());
    }

    #[test]
    fn cancelled_outcome_returns_none() {
        let args = r#"{"path":"src/main.rs"}"#;
        assert!(derive_display_for_replay(READ_TOOL_NAME, args, &ToolResultOutcome::Cancelled).is_none());
    }

    #[test]
    fn interrupted_outcome_returns_none() {
        let args = r#"{"path":"src/main.rs"}"#;
        assert!(derive_display_for_replay(READ_TOOL_NAME, args, &ToolResultOutcome::Interrupted).is_none());
    }

    #[test]
    fn error_outcome_returns_none() {
        let args = r#"{"path":"src/main.rs"}"#;
        assert!(derive_display_for_replay(READ_TOOL_NAME, args, &error("not_found", "missing")).is_none());
    }

    #[test]
    fn empty_arguments_do_not_panic() {
        let display = derive_display_for_replay(READ_TOOL_NAME, "", &success("ok"))
            .expect("read still derives with empty args");
        assert_eq!(fact_value(&display, "subject"), Some(""));
    }

    #[test]
    fn empty_content_does_not_panic() {
        let display = derive_display_for_replay(READ_TOOL_NAME, r#"{"path":"x"}"#, &success(""))
            .expect("read still derives with empty content");
        assert_eq!(fact_value(&display, "lines_shown"), Some("0"));
    }

    #[test]
    fn each_tool_emits_title_verb_body_in_order() {
        let cases = [
            (READ_TOOL_NAME, r#"{"path":"x"}"#, "ok"),
            (GREP_TOOL_NAME, r#"{"pattern":"x"}"#, "src:1: x"),
            (LIST_TOOL_NAME, r#"{"path":"x"}"#, "file\tx"),
            (WRITE_TOOL_NAME, r#"{"path":"x","content":"y"}"#, "created x"),
            (EDIT_TOOL_NAME, r#"{"path":"x","old_string":"a","new_string":"b"}"#, "edited x"),
            (SHELL_TOOL_NAME, r#"{"command":"x"}"#, "exit_code: 0"),
        ];
        for (name, args, content) in cases {
            let display = derive_display_for_replay(name, args, &success(content))
                .unwrap_or_else(|| panic!("{name} should derive a display"));
            let names = fact_names(&display);
            assert_eq!(
                &names[..3],
                &["title", "verb", "body"],
                "{name} must emit title/verb/body first"
            );
        }
    }
}
