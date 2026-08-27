//! Root-constrained text reader with line numbers, `offset`/`limit`
//! paging, and a byte/line dual truncation limit (M10 upgrade of the M4
//! tool).

use std::path::PathBuf;

use philo_tools::{
    EffectClass, RichToolResult, ToolArguments, ToolDefinition, ToolHandler, ToolHandlerEndFuture,
    ToolHandlerFuture, ToolInvokeCx, ToolInvokeEnd,
};

use crate::args::{optional_u64, required_string};
use crate::display::{CardFacts, card};
use crate::error_code;
use crate::helpers::{field_error, io_error, not_found, path_error, stopped_if_cancelled};
use crate::path::resolve_in_root;

/// Stable registry name of the read tool.
pub const READ_TOOL_NAME: &str = "read";

/// Default upper bound of returned file content, in bytes.
pub const DEFAULT_MAX_READ_BYTES: usize = 64 * 1024;

/// Default upper bound of returned lines.
pub const DEFAULT_MAX_READ_LINES: usize = 2000;

/// File extensions the read tool refuses as images (suggesting `--image`).
const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// Text reader constrained to a root directory. Output lines are prefixed
/// with their 1-based line number; `offset`/`limit` page through large
/// files, and content beyond the byte or line limit is truncated with an
/// actionable continuation marker.
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
             prefixes; use offset (1-based line number) and limit to page \
             through large files — the truncation marker states the next \
             offset. Images and other binary files are rejected.",
            r#"{"type":"object","properties":{"path":{"type":"string","description":"File path to read, resolved against the root directory"},"offset":{"type":"integer","description":"Optional 1-based line number to start reading from"},"limit":{"type":"integer","description":"Optional maximum number of lines to read"}},"required":["path"]}"#,
            EffectClass::ReadOnly,
        )
        .expect("read tool definition is valid")
    }

    fn read(&self, arguments: &ToolArguments) -> RichToolResult {
        let path = match required_string(arguments.as_str(), "path") {
            Ok(path) => path,
            Err(error) => return field_error("path", &error),
        };
        let offset = match optional_u64(arguments.as_str(), "offset") {
            Ok(offset) => offset,
            Err(error) => return field_error("offset", &error),
        };
        let limit = match optional_u64(arguments.as_str(), "limit") {
            Ok(limit) => limit,
            Err(error) => return field_error("limit", &error),
        };
        let start_line = usize::try_from(offset.unwrap_or(1).max(1)).unwrap_or(usize::MAX);
        let effective_lines = limit.map_or(self.max_lines, |value| {
            usize::try_from(value)
                .unwrap_or(usize::MAX)
                .clamp(1, self.max_lines)
        });
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

        // Model channel: line numbers from the requested offset, then the
        // dual limit. Truncation is final here — nothing above the tool
        // boundary re-truncates.
        let lines_total = text.lines().count();
        if lines_total == 0 {
            let display = card("Read", "Read", "none", "")
                .subject(&path)
                .count("1 file")
                .with_fact("start_line", "0")
                .with_fact("end_line", "0")
                .with_fact("lines_shown", "0")
                .with_fact("lines_total", "0")
                .with_fact("bytes_total", total_bytes.to_string())
                .with_fact("truncated", "false");
            return RichToolResult::new(philo_tools::ToolResult::success(
                "(empty file)".to_owned(),
            ))
            .with_display(display);
        }
        if start_line > lines_total {
            return RichToolResult::error(
                error_code::INVALID_ARGUMENTS,
                format!("offset {start_line} is beyond end of file ({lines_total} lines)"),
            );
        }

        let mut numbered = String::new();
        let mut emitted_lines = 0usize;
        let mut stopped_by_budget = false;
        let mut stopped_by_limit_param = false;
        for (index, line) in text.lines().enumerate().skip(start_line - 1) {
            if emitted_lines >= effective_lines {
                stopped_by_limit_param = true;
                break;
            }
            let row = format!("{:>5}|{}\n", index + 1, line);
            if numbered.len() + row.len() > self.max_bytes {
                stopped_by_budget = true;
                break;
            }
            numbered.push_str(&row);
            emitted_lines += 1;
        }
        let end_line = start_line + emitted_lines.saturating_sub(1);

        let mut model_text = numbered;
        let truncated;
        if emitted_lines == 0 && stopped_by_budget {
            truncated = true;
            model_text.push_str(&format!(
                "[read truncated: line {start_line} alone exceeds the {}-byte \
                 output budget; use shell to slice it]",
                self.max_bytes
            ));
        } else if stopped_by_budget {
            truncated = true;
            model_text.push_str(&format!(
                "[read truncated: showing lines {start_line}-{end_line} of \
                 {lines_total}; use offset={} to continue]",
                end_line + 1
            ));
        } else {
            let remaining = lines_total - end_line;
            truncated = false;
            if stopped_by_limit_param && remaining > 0 {
                model_text.push_str(&format!(
                    "[read stopped by limit at line {end_line}; {remaining} more \
                     lines; use offset={} to continue]",
                    end_line + 1
                ));
            }
        }

        let display = card("Read", "Read", "none", "")
            .subject(&path)
            .count("1 file")
            .with_fact("start_line", start_line.to_string())
            .with_fact("end_line", end_line.to_string())
            .with_fact("lines_shown", emitted_lines.to_string())
            .with_fact("lines_total", lines_total.to_string())
            .with_fact("bytes_total", total_bytes.to_string())
            .with_fact("truncated", truncated.to_string());
        RichToolResult::new(philo_tools::ToolResult::success(model_text)).with_display(display)
    }
}

impl ToolHandler for ReadTool {
    fn call<'a>(&'a self, arguments: ToolArguments) -> ToolHandlerFuture<'a> {
        Box::pin(async move { self.read(&arguments) })
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
            ToolInvokeEnd::Done(self.read(&arguments))
        })
    }
}
