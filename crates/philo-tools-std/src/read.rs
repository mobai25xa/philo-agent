//! Root-constrained whole-file text reader with line numbers and a
//! byte/line dual truncation limit (M10 upgrade of the M4 tool).

use std::path::PathBuf;

use philo_tools::{
    EffectClass, RichToolResult, ToolArguments, ToolDefinition, ToolDisplay, ToolHandler,
    ToolHandlerFuture,
};

use crate::args::required_string;
use crate::error_code;
use crate::helpers::{field_error, io_error, not_found, path_error};
use crate::path::resolve_in_root;

/// Stable registry name of the read tool.
pub const READ_TOOL_NAME: &str = "read";

/// Default upper bound of returned file content, in bytes.
pub const DEFAULT_MAX_READ_BYTES: usize = 64 * 1024;

/// Default upper bound of returned lines.
pub const DEFAULT_MAX_READ_LINES: usize = 2000;

/// File extensions the read tool refuses as images (suggesting `--image`).
const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// Whole-file text reader constrained to a root directory. Output lines are
/// prefixed with their 1-based line number; content beyond the byte or line
/// limit is truncated with an explicit marker.
pub struct ReadTool {
    root: PathBuf,
    max_bytes: usize,
    max_lines: usize,
}

impl ReadTool {
    /// Creates a read tool rooted at `root` with default limits.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_bytes: DEFAULT_MAX_READ_BYTES,
            max_lines: DEFAULT_MAX_READ_LINES,
        }
    }

    /// Overrides the byte limit (minimum 1).
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes.max(1);
        self
    }

    /// Overrides the line limit (minimum 1).
    pub fn with_max_lines(mut self, max_lines: usize) -> Self {
        self.max_lines = max_lines.max(1);
        self
    }

    /// Returns the model-facing definition registered for this tool.
    pub fn definition() -> ToolDefinition {
        ToolDefinition::new(
            READ_TOOL_NAME,
            "Read a UTF-8 text file located inside the workspace root. Relative \
             paths resolve against the root. Output lines carry line-number \
             prefixes; content beyond the size limits is truncated with a marker. \
             Images and other binary files are rejected.",
            r#"{"type":"object","properties":{"path":{"type":"string","description":"File path to read, resolved against the root directory"}},"required":["path"]}"#,
            EffectClass::ReadOnly,
        )
        .expect("read tool definition is valid")
    }

    fn read(&self, arguments: &ToolArguments) -> RichToolResult {
        let path = match required_string(arguments.as_str(), "path") {
            Ok(path) => path,
            Err(error) => return field_error("path", &error),
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
        let total_bytes = bytes.len();

        let is_image = target
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                IMAGE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
            });
        if is_image {
            return RichToolResult::error(
                error_code::BINARY_FILE,
                format!(
                    "'{path}' is an image file; attach it to the user message \
                     (for example with --image) instead of reading it as text"
                ),
            );
        }
        if bytes.contains(&0) {
            return RichToolResult::error(
                error_code::BINARY_FILE,
                format!("'{path}' is a binary file and cannot be read as text"),
            );
        }
        let Ok(text) = String::from_utf8(bytes) else {
            return RichToolResult::error(
                error_code::NOT_UTF8,
                format!("file is not valid UTF-8 text: {path}"),
            );
        };

        // Model channel: line numbers, then the dual limit. Truncation is
        // final here — nothing above the tool boundary re-truncates.
        let lines_total = text.lines().count();
        let mut numbered = String::new();
        let mut emitted_lines = 0usize;
        let mut truncated = false;
        for (index, line) in text.lines().enumerate() {
            let row = format!("{:>5}|{}\n", index + 1, line);
            if emitted_lines >= self.max_lines || numbered.len() + row.len() > self.max_bytes {
                truncated = true;
                break;
            }
            numbered.push_str(&row);
            emitted_lines += 1;
        }
        let mut model_text = numbered;
        if truncated {
            model_text.push_str(&format!(
                "[read truncated: showing first {emitted_lines} of {lines_total} lines \
                 ({total_bytes} bytes total)]"
            ));
        }

        let display = ToolDisplay::new(format!(
            "read {path}: {emitted_lines} of {lines_total} lines shown"
        ))
        .with_fact("bytes_total", total_bytes.to_string())
        .with_fact("lines_total", lines_total.to_string())
        .with_fact("truncated", truncated.to_string());
        RichToolResult::new(philo_tools::ToolResult::success(model_text)).with_display(display)
    }
}

impl ToolHandler for ReadTool {
    fn call<'a>(&'a self, arguments: ToolArguments) -> ToolHandlerFuture<'a> {
        Box::pin(async move { self.read(&arguments) })
    }
}
