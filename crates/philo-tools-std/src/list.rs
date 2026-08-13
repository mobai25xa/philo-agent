//! Root-constrained single-level directory listing with optional glob
//! filtering and an entry-count truncation limit.

use std::path::PathBuf;

use philo_tools::{
    EffectClass, RichToolResult, ToolArguments, ToolDefinition, ToolDisplay, ToolHandler,
    ToolHandlerFuture, ToolResult,
};

use crate::args::{optional_string, required_string};
use crate::error_code;
use crate::helpers::{field_error, io_error, path_error};
use crate::path::resolve_in_root;

/// Stable registry name of the list tool.
pub const LIST_TOOL_NAME: &str = "list";

/// Default upper bound of returned directory entries.
pub const DEFAULT_MAX_LIST_ENTRIES: usize = 500;

/// Single-level directory listing constrained to a root directory. Entries
/// are typed (`dir` / `file`), sorted directories-first then by name, and
/// optionally filtered by a glob over the entry name.
pub struct ListTool {
    root: PathBuf,
    max_entries: usize,
}

impl ListTool {
    /// Creates a list tool rooted at `root` with the default entry limit.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_entries: DEFAULT_MAX_LIST_ENTRIES,
        }
    }

    /// Overrides the entry limit (minimum 1).
    pub fn with_max_entries(mut self, max_entries: usize) -> Self {
        self.max_entries = max_entries.max(1);
        self
    }

    /// Returns the model-facing definition registered for this tool.
    pub fn definition() -> ToolDefinition {
        ToolDefinition::new(
            LIST_TOOL_NAME,
            "List the entries of a directory inside the workspace root (one \
             level, not recursive). Each line is 'dir<TAB>name' or \
             'file<TAB>name'. An optional glob filters entry names.",
            r#"{"type":"object","properties":{"path":{"type":"string","description":"Directory to list, resolved against the root; defaults to the root itself"},"glob":{"type":"string","description":"Optional glob filtering entry names, e.g. *.rs"}},"required":["path"]}"#,
            EffectClass::ReadOnly,
        )
        .expect("list tool definition is valid")
    }

    fn list(&self, arguments: &ToolArguments) -> RichToolResult {
        let path = match required_string(arguments.as_str(), "path") {
            Ok(path) => path,
            Err(error) => return field_error("path", &error),
        };
        let glob = match optional_string(arguments.as_str(), "glob") {
            Ok(glob) => glob,
            Err(error) => return field_error("glob", &error),
        };
        let matcher = match glob.as_deref() {
            None => None,
            Some(pattern) => match globset::Glob::new(pattern) {
                Ok(glob) => Some(glob.compile_matcher()),
                Err(error) => {
                    return RichToolResult::error(
                        error_code::INVALID_GLOB,
                        format!("invalid glob '{pattern}': {error}"),
                    );
                }
            },
        };

        let target = match resolve_in_root(&self.root, &path, true) {
            Ok(target) => target,
            Err(error) => return path_error(&path, &error),
        };
        if !target.is_dir() {
            return RichToolResult::error(
                error_code::NOT_A_DIRECTORY,
                format!("path is not a directory: {path}"),
            );
        }

        let reader = match std::fs::read_dir(&target) {
            Ok(reader) => reader,
            Err(error) => return io_error(error.kind()),
        };
        let mut entries: Vec<(bool, String)> = Vec::new();
        for entry in reader {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(matcher) = &matcher
                && !matcher.is_match(&name)
            {
                continue;
            }
            let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            entries.push((is_dir, name));
        }
        // Deterministic order: directories first, then names.
        entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

        let total = entries.len();
        let truncated = total > self.max_entries;
        let mut lines = Vec::with_capacity(entries.len().min(self.max_entries));
        for (is_dir, name) in entries.into_iter().take(self.max_entries) {
            lines.push(format!("{}\t{name}", if is_dir { "dir" } else { "file" }));
        }
        let shown = lines.len();
        let mut model_text = lines.join("\n");
        if truncated {
            if !model_text.is_empty() {
                model_text.push('\n');
            }
            model_text.push_str(&format!(
                "[list truncated: showing first {shown} of {total} entries]"
            ));
        }

        let display = ToolDisplay::new(format!("listed {path}: {shown} of {total} entries shown"))
            .with_fact("entries_total", total.to_string())
            .with_fact("truncated", truncated.to_string());
        RichToolResult::new(ToolResult::success(model_text)).with_display(display)
    }
}

impl ToolHandler for ListTool {
    fn call<'a>(&'a self, arguments: ToolArguments) -> ToolHandlerFuture<'a> {
        Box::pin(async move { self.list(&arguments) })
    }
}
