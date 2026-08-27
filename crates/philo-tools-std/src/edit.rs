//! Root-constrained exact-string replacement: `old_string` must occur
//! exactly once; zero and multiple occurrences are distinguishable errors.
//! Matching is line-ending tolerant: content and `old_string` are normalized
//! to LF, and the file's original line endings (plus any BOM) are restored
//! on write.

use std::path::PathBuf;

use philo_tools::{
    EffectClass, RichToolResult, ToolArguments, ToolDefinition, ToolHandler, ToolHandlerEndFuture,
    ToolHandlerFuture, ToolInvokeCx, ToolInvokeEnd, ToolResult,
};

use crate::args::required_string;
use crate::display::{CardFacts, card, edit_hunk};
use crate::error_code;
use crate::helpers::{field_error, io_error, not_found, path_error, stopped_if_cancelled};
use crate::mutation::with_file_mutation;
use crate::path::resolve_in_root;

/// Stable registry name of the edit tool.
pub const EDIT_TOOL_NAME: &str = "edit";

/// Exact single-occurrence string replacement constrained to a root
/// directory.
pub struct EditTool {
    root: PathBuf,
}

impl EditTool {
    /// Creates an edit tool rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns the model-facing definition registered for this tool.
    pub fn definition() -> ToolDefinition {
        ToolDefinition::new(
            EDIT_TOOL_NAME,
            "Replace one exact string occurrence in a UTF-8 text file inside \
             the workspace root. old_string must match exactly once: zero \
             matches and multiple matches are distinct errors (add more \
             surrounding context to disambiguate). new_string may be empty to \
             delete the matched text. Line endings are matched tolerantly \
             (LF matches CRLF files) and the file's endings are preserved.",
            r#"{"type":"object","properties":{"path":{"type":"string","description":"File path to edit, resolved against the root directory"},"old_string":{"type":"string","description":"Exact text to replace; must occur exactly once"},"new_string":{"type":"string","description":"Replacement text; empty deletes the match"}},"required":["path","old_string","new_string"]}"#,
            EffectClass::Workspace,
        )
        .expect("edit tool definition is valid")
    }

    fn edit(&self, arguments: &ToolArguments) -> RichToolResult {
        let path = match required_string(arguments.as_str(), "path") {
            Ok(path) => path,
            Err(error) => return field_error("path", &error),
        };
        let old_string = match required_string(arguments.as_str(), "old_string") {
            Ok(value) => value,
            Err(error) => return field_error("old_string", &error),
        };
        if old_string.is_empty() {
            return RichToolResult::error(
                error_code::INVALID_ARGUMENTS,
                "argument 'old_string' must not be empty",
            );
        }
        let new_string = match required_string(arguments.as_str(), "new_string") {
            Ok(value) => value,
            Err(error) => return field_error("new_string", &error),
        };

        let target = match resolve_in_root(&self.root, &path, true) {
            Ok(target) => target,
            Err(error) => return path_error(&path, &error),
        };
        if target.is_dir() {
            return RichToolResult::error(
                error_code::NOT_A_FILE,
                format!("path is a directory, not a file: {path}"),
            );
        }

        // The read-match-write cycle holds the per-path mutation lock so a
        // concurrent edit/write on the same file cannot interleave.
        with_file_mutation(&target, || {
            let bytes = match std::fs::read(&target) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return not_found(&path);
                }
                Err(error) => return io_error(error.kind()),
            };
            let Ok(full_text) = String::from_utf8(bytes) else {
                return RichToolResult::error(
                    error_code::NOT_UTF8,
                    format!("file is not valid UTF-8 text: {path}"),
                );
            };

            // Strip the BOM before matching; models never include it in
            // old_string. It is re-attached verbatim on write.
            let (bom, content) = match full_text.strip_prefix('\u{FEFF}') {
                Some(rest) => ("\u{FEFF}", rest),
                None => ("", full_text.as_str()),
            };
            // Normalize line endings for matching so an LF old_string hits a
            // CRLF file; the original ending is restored on write.
            let crlf = content.contains("\r\n");
            let normalized = normalize_to_lf(content);
            let old_normalized = normalize_to_lf(&old_string);

            let occurrences = normalized.matches(&old_normalized).count();
            if occurrences == 0 {
                return RichToolResult::error(
                    error_code::NO_MATCH,
                    format!("old_string not found in {path}"),
                );
            }
            if occurrences > 1 {
                return RichToolResult::error(
                    error_code::NOT_UNIQUE,
                    format!(
                        "old_string occurs {occurrences} times in {path}; add more \
                         surrounding context to make it unique"
                    ),
                );
            }
            let edited = normalized.replacen(&old_normalized, &new_string, 1);
            let restored = if crlf {
                edited.replace('\n', "\r\n")
            } else {
                edited
            };
            let final_text = format!("{bom}{restored}");
            if let Err(error) = std::fs::write(&target, final_text.as_bytes()) {
                return io_error(error.kind());
            }

            let confirmation = format!(
                "edited {path}: replaced 1 occurrence ({} -> {} bytes)",
                full_text.len(),
                final_text.len()
            );
            let (hunk, added, removed) = edit_hunk(&normalized, &old_normalized, &new_string);
            let display = card("Edit", "Edited", "diff", hunk)
                .subject(&path)
                .result(format!(
                    "Succeeded. File edited.  (+{added} added, -{removed} removed)"
                ))
                .with_fact("added", added.to_string())
                .with_fact("removed", removed.to_string())
                .with_fact("bytes_before", full_text.len().to_string())
                .with_fact("bytes_after", final_text.len().to_string());
            RichToolResult::new(ToolResult::success(confirmation)).with_display(display)
        })
    }
}

fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

impl ToolHandler for EditTool {
    fn call<'a>(&'a self, arguments: ToolArguments) -> ToolHandlerFuture<'a> {
        Box::pin(async move { self.edit(&arguments) })
    }

    fn call_with_cx<'a>(
        &'a self,
        arguments: ToolArguments,
        cx: ToolInvokeCx,
    ) -> ToolHandlerEndFuture<'a> {
        Box::pin(async move {
            if let Some(stopped) = stopped_if_cancelled(&cx) {
                return stopped;
            }
            ToolInvokeEnd::Done(self.edit(&arguments))
        })
    }
}
