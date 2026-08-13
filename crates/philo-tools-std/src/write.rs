//! Root-constrained whole-file write with parent-directory creation and an
//! explicit created/overwrote confirmation.

use std::path::PathBuf;

use philo_tools::{
    EffectClass, RichToolResult, ToolArguments, ToolDefinition, ToolDisplay, ToolHandler,
    ToolHandlerFuture, ToolResult,
};

use crate::args::required_string;
use crate::error_code;
use crate::helpers::{field_error, io_error, path_error};
use crate::path::resolve_in_root;

/// Stable registry name of the write tool.
pub const WRITE_TOOL_NAME: &str = "write";

/// Whole-file writer constrained to a root directory: full overwrite
/// semantics, missing parent directories are created.
pub struct WriteTool {
    root: PathBuf,
}

impl WriteTool {
    /// Creates a write tool rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns the model-facing definition registered for this tool.
    pub fn definition() -> ToolDefinition {
        ToolDefinition::new(
            WRITE_TOOL_NAME,
            "Write a UTF-8 text file inside the workspace root, replacing its \
             whole content. Missing parent directories are created. The result \
             confirms whether the file was created or overwritten.",
            r#"{"type":"object","properties":{"path":{"type":"string","description":"File path to write, resolved against the root directory"},"content":{"type":"string","description":"The complete new file content"}},"required":["path","content"]}"#,
            EffectClass::Workspace,
        )
        .expect("write tool definition is valid")
    }

    fn write(&self, arguments: &ToolArguments) -> RichToolResult {
        let path = match required_string(arguments.as_str(), "path") {
            Ok(path) => path,
            Err(error) => return field_error("path", &error),
        };
        let content = match required_string(arguments.as_str(), "content") {
            Ok(content) => content,
            Err(error) => return field_error("content", &error),
        };
        let target = match resolve_in_root(&self.root, &path, false) {
            Ok(target) => target,
            Err(error) => return path_error(&path, &error),
        };
        if target.is_dir() {
            return RichToolResult::error(
                error_code::NOT_A_FILE,
                format!("path is a directory, not a file: {path}"),
            );
        }

        let previous_bytes = std::fs::metadata(&target).ok().map(|meta| meta.len());
        if let Some(parent) = target.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            return io_error(error.kind());
        }
        if let Err(error) = std::fs::write(&target, content.as_bytes()) {
            return io_error(error.kind());
        }

        let bytes = content.len();
        let confirmation = match previous_bytes {
            None => format!("created {path} ({bytes} bytes)"),
            Some(was) => format!("overwrote {path} ({bytes} bytes, was {was} bytes)"),
        };
        let display = ToolDisplay::new(format!("wrote {path}:\n{content}"))
            .with_fact("bytes", bytes.to_string())
            .with_fact(
                "operation",
                if previous_bytes.is_none() {
                    "created"
                } else {
                    "overwrote"
                },
            );
        RichToolResult::new(ToolResult::success(confirmation)).with_display(display)
    }
}

impl ToolHandler for WriteTool {
    fn call<'a>(&'a self, arguments: ToolArguments) -> ToolHandlerFuture<'a> {
        Box::pin(async move { self.write(&arguments) })
    }
}
