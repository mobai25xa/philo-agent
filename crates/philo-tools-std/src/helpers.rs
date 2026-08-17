//! Shared error-construction helpers.
//!
//! Tool business failures are always `ToolResult::Error` with a stable code.
//! The only `ToolPortError` paths in this crate are blocking-pool saturation
//! and a panicked `spawn_blocking` worker (see [`crate::BlockingPool`]).

use philo_tools::{RichToolResult, ToolCancel, ToolInvokeCx, ToolInvokeEnd};

use crate::args::FieldError;
use crate::error_code;
use crate::path::PathError;

/// Returns [`ToolInvokeEnd::Stopped`] when the invoke token is already requested.
pub(crate) fn stopped_if_cancelled(cx: &ToolInvokeCx) -> Option<ToolInvokeEnd> {
    stopped_if_requested(cx.cancel())
}

/// Returns [`ToolInvokeEnd::Stopped`] when `cancel` has been requested.
pub(crate) fn stopped_if_requested(cancel: &ToolCancel) -> Option<ToolInvokeEnd> {
    if cancel.is_requested() {
        Some(ToolInvokeEnd::Stopped)
    } else {
        None
    }
}

pub(crate) fn field_error(key: &str, error: &FieldError) -> RichToolResult {
    let message = match error {
        FieldError::Missing => format!("missing required argument: {key}"),
        FieldError::NotAString => format!("argument '{key}' must be a string"),
        FieldError::NotANumber => format!("argument '{key}' must be a non-negative integer"),
        FieldError::BadEscape => format!("argument '{key}' contains an invalid escape sequence"),
    };
    RichToolResult::error(error_code::INVALID_ARGUMENTS, message)
}

pub(crate) fn path_error(path: &str, error: &PathError) -> RichToolResult {
    match error {
        PathError::OutsideRoot => outside_root(path),
        PathError::NotFound => not_found(path),
        PathError::Io(kind) => io_error(*kind),
    }
}

pub(crate) fn outside_root(path: &str) -> RichToolResult {
    RichToolResult::error(
        error_code::OUTSIDE_ROOT,
        format!("path resolves outside the root directory: {path}"),
    )
}

pub(crate) fn not_found(path: &str) -> RichToolResult {
    RichToolResult::error(error_code::NOT_FOUND, format!("file not found: {path}"))
}

pub(crate) fn io_error(kind: std::io::ErrorKind) -> RichToolResult {
    RichToolResult::error(error_code::IO_ERROR, format!("filesystem error ({kind:?})"))
}
