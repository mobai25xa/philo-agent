//! Root-constrained exact-string replacement: `old_string` must occur
//! exactly once; zero and multiple occurrences are distinguishable errors.

use std::path::PathBuf;

use philo_tools::{
    EffectClass, RichToolResult, ToolArguments, ToolDefinition, ToolDisplay, ToolHandler,
    ToolHandlerEndFuture, ToolHandlerFuture, ToolInvokeCx, ToolInvokeEnd, ToolResult,
};

use crate::args::required_string;
use crate::error_code;
use crate::helpers::{field_error, io_error, not_found, path_error, stopped_if_cancelled};
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
             delete the matched text.",
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
        let bytes = match std::fs::read(&target) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return not_found(&path);
            }
            Err(error) => return io_error(error.kind()),
        };
        let Ok(text) = String::from_utf8(bytes) else {
            return RichToolResult::error(
                error_code::NOT_UTF8,
                format!("file is not valid UTF-8 text: {path}"),
            );
        };

        let occurrences = text.matches(&old_string).count();
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
        let edited = text.replacen(&old_string, &new_string, 1);
        if let Err(error) = std::fs::write(&target, edited.as_bytes()) {
            return io_error(error.kind());
        }

        let confirmation = format!(
            "edited {path}: replaced 1 occurrence ({} -> {} bytes)",
            text.len(),
            edited.len()
        );
        let display = ToolDisplay::new(format!("--- old\n{old_string}\n+++ new\n{new_string}"))
            .with_fact("bytes_before", text.len().to_string())
            .with_fact("bytes_after", edited.len().to_string());
        RichToolResult::new(ToolResult::success(confirmation)).with_display(display)
    }
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
