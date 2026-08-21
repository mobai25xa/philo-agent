//! Root-constrained single-level directory listing with optional glob
//! filtering and dual entry-count/byte truncation limits.

use std::path::PathBuf;

use philo_tools::{
    EffectClass, RichToolResult, ToolArguments, ToolCancel, ToolDefinition, ToolHandler,
    ToolHandlerEndFuture, ToolHandlerFuture, ToolInvokeCx, ToolInvokeEnd, ToolResult,
};

use crate::args::{optional_string, optional_u64};
use crate::display::card;
use crate::error_code;
use crate::helpers::{field_error, io_error, path_error, stopped_if_requested};
use crate::path::resolve_in_root;

/// Stable registry name of the list tool.
pub const LIST_TOOL_NAME: &str = "list";

/// Default upper bound of returned directory entries.
pub const DEFAULT_MAX_LIST_ENTRIES: usize = 500;

/// Default upper bound of returned listing bytes.
pub const DEFAULT_MAX_LIST_BYTES: usize = 64 * 1024;

/// Single-level directory listing constrained to a root directory. Entries
/// are typed (`dir` / `file`), sorted directories-first then by name
/// (case-insensitive), and optionally filtered by a glob over the entry name.
pub struct ListTool {
    root: PathBuf,
    max_entries: usize,
    max_bytes: usize,
}

impl ListTool {
    /// Creates a list tool rooted at `root` with the default limits.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_entries: DEFAULT_MAX_LIST_ENTRIES,
            max_bytes: DEFAULT_MAX_LIST_BYTES,
        }
    }

    /// Overrides the entry limit (minimum 1).
    pub fn with_max_entries(mut self, max_entries: usize) -> Self {
        self.max_entries = max_entries.max(1);
        self
    }

    /// Overrides the byte limit (minimum 1).
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes.max(1);
        self
    }

    /// Returns the model-facing definition registered for this tool.
    pub fn definition() -> ToolDefinition {
        ToolDefinition::new(
            LIST_TOOL_NAME,
            "List the entries of a directory inside the workspace root (one \
             level, not recursive). Each line is 'dir<TAB>name' or \
             'file<TAB>name'. An optional glob filters entry names. Output is \
             capped by an entry limit (default 500) and a byte cap; the \
             truncation marker states how to see more.",
            r#"{"type":"object","properties":{"path":{"type":"string","description":"Directory to list, resolved against the root; defaults to the root itself"},"glob":{"type":"string","description":"Optional glob filtering entry names, e.g. *.rs"},"limit":{"type":"integer","description":"Optional maximum number of entries to return (capped by the configured limit)"}}}"#,
            EffectClass::ReadOnly,
        )
        .expect("list tool definition is valid")
    }

    fn list(&self, arguments: &ToolArguments, cancel: &ToolCancel) -> ToolInvokeEnd {
        if let Some(stopped) = stopped_if_requested(cancel) {
            return stopped;
        }
        let path = match optional_string(arguments.as_str(), "path") {
            Ok(path) => path.unwrap_or_else(|| ".".to_owned()),
            Err(error) => return ToolInvokeEnd::Done(field_error("path", &error)),
        };
        let glob = match optional_string(arguments.as_str(), "glob") {
            Ok(glob) => glob,
            Err(error) => return ToolInvokeEnd::Done(field_error("glob", &error)),
        };
        let matcher = match glob.as_deref() {
            None => None,
            Some(pattern) => match globset::Glob::new(pattern) {
                Ok(glob) => Some(glob.compile_matcher()),
                Err(error) => {
                    return ToolInvokeEnd::Done(RichToolResult::error(
                        error_code::INVALID_GLOB,
                        format!("invalid glob '{pattern}': {error}"),
                    ));
                }
            },
        };
        let effective_limit = match optional_u64(arguments.as_str(), "limit") {
            Ok(Some(value)) => usize::try_from(value)
                .unwrap_or(usize::MAX)
                .clamp(1, self.max_entries),
            Ok(None) => self.max_entries,
            Err(error) => return ToolInvokeEnd::Done(field_error("limit", &error)),
        };

        let target = match resolve_in_root(&self.root, &path, true) {
            Ok(target) => target,
            Err(error) => return ToolInvokeEnd::Done(path_error(&path, &error)),
        };
        if !target.is_dir() {
            return ToolInvokeEnd::Done(RichToolResult::error(
                error_code::NOT_A_DIRECTORY,
                format!("path is not a directory: {path}"),
            ));
        }

        if let Some(stopped) = stopped_if_requested(cancel) {
            return stopped;
        }
        let reader = match std::fs::read_dir(&target) {
            Ok(reader) => reader,
            Err(error) => return ToolInvokeEnd::Done(io_error(error.kind())),
        };
        // (is_dir, name, lowercase name): the lowercase key drives the sort.
        let mut entries: Vec<(bool, String, String)> = Vec::new();
        for entry in reader {
            if let Some(stopped) = stopped_if_requested(cancel) {
                return stopped;
            }
            let Ok(entry) = entry else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(matcher) = &matcher
                && !matcher.is_match(&name)
            {
                continue;
            }
            // file_type() does not follow symlinks: re-stat link entries so a
            // symlinked directory is still typed as dir. Entries whose type
            // cannot be determined are skipped rather than misreported.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let is_dir = if file_type.is_symlink() {
                match entry.metadata() {
                    Ok(metadata) => metadata.is_dir(),
                    Err(_) => continue,
                }
            } else {
                file_type.is_dir()
            };
            let lower = name.to_lowercase();
            entries.push((is_dir, name, lower));
        }
        // Deterministic order: directories first, then case-insensitive names
        // (raw-name comparison breaks case-only ties stably).
        entries.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| a.1.cmp(&b.1))
        });

        let total = entries.len();
        let mut lines: Vec<String> = Vec::new();
        let mut used_bytes = 0usize;
        for (is_dir, name, _) in &entries {
            if lines.len() >= effective_limit {
                break;
            }
            let row = format!("{}\t{name}", if *is_dir { "dir" } else { "file" });
            let row_len = row.len() + 1;
            if used_bytes + row_len > self.max_bytes {
                break;
            }
            used_bytes += row_len;
            lines.push(row);
        }
        let shown = lines.len();
        let truncated = total > shown;
        let mut model_text = if total == 0 {
            "(empty directory)".to_owned()
        } else {
            lines.join("\n")
        };
        if truncated {
            if !model_text.is_empty() {
                model_text.push('\n');
            }
            model_text.push_str(&format!(
                "[list truncated: showing first {shown} of {total} entries; \
                 raise \"limit\" (max {}) or narrow the path/glob]",
                self.max_entries
            ));
        }

        let display = card("Listed", path, "none", "")
            .with_fact("entries_total", total.to_string())
            .with_fact("truncated", truncated.to_string());
        ToolInvokeEnd::Done(
            RichToolResult::new(ToolResult::success(model_text)).with_display(display),
        )
    }
}

impl ToolHandler for ListTool {
    fn call<'a>(&'a self, arguments: ToolArguments) -> ToolHandlerFuture<'a> {
        Box::pin(async move {
            self.list(&arguments, &ToolCancel::none())
                .into_done()
                .expect("list cannot stop without a requested cancel")
        })
    }

    fn call_with_cx<'a>(
        &'a self,
        arguments: ToolArguments,
        cx: ToolInvokeCx,
    ) -> ToolHandlerEndFuture<'a> {
        Box::pin(async move { self.list(&arguments, cx.cancel()) })
    }
}
