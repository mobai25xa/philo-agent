//! Standard coding tools for the agent workspace (M10).
//!
//! Six tools cover the coding query-modify-verify loop: `read`, `list`,
//! `grep` (ReadOnly), `write`, `edit` (Workspace), and `shell` (System).
//! Every handler produces a dual-channel [`philo_tools::RichToolResult`]:
//! the model channel is truncated and finalized inside the handler (the
//! durable fact equals what the model sees), the display channel carries
//! bounded human-readable detail for transient presentation.
//!
//! Every failure here is a business error (`ToolResult::Error` with a
//! stable code); this crate never raises `ToolPortError` infrastructure
//! failures. Path-taking tools are constrained to the assembly-injected
//! workspace root with a two-phase containment check (lexical, then
//! canonical — symlink escapes are rejected).
//!
//! The `shell` tool needs a tokio runtime (io-util + process + time); composition
//! roots provide one (`philo-cli` does). The other five tools are
//! runtime-agnostic.
//!
//! # Stable business-error codes
//!
//! | code | meaning |
//! |---|---|
//! | `invalid_arguments` | missing/mistyped argument, bad escape, zero/oversized timeout, empty old_string |
//! | `outside_root` | path resolves outside the workspace root (incl. symlink escape) |
//! | `not_found` | file or directory does not exist |
//! | `not_a_file` | path is a directory where a file is required |
//! | `not_a_directory` | path is a file where a directory is required |
//! | `not_utf8` | file content is not valid UTF-8 text |
//! | `binary_file` | file is binary (or an image: attach via `--image` instead) |
//! | `invalid_glob` | glob pattern does not compile |
//! | `invalid_regex` | regular expression does not compile |
//! | `no_match` | edit old_string not found |
//! | `not_unique` | edit old_string occurs more than once |
//! | `timeout` | shell command exceeded its time limit and was terminated |
//! | `spawn_failed` | the platform shell could not be started |
//! | `io_error` | any other filesystem I/O failure |

mod args;
mod display;
mod edit;
mod grep;
mod helpers;
mod list;
mod path;
mod read;
mod shell;
mod write;

pub use edit::{EDIT_TOOL_NAME, EditTool};
pub use grep::{DEFAULT_MAX_GREP_MATCHES, GREP_TOOL_NAME, GrepTool};
pub use list::{DEFAULT_MAX_LIST_ENTRIES, LIST_TOOL_NAME, ListTool};
pub use read::{DEFAULT_MAX_READ_BYTES, DEFAULT_MAX_READ_LINES, READ_TOOL_NAME, ReadTool};
pub use shell::{
    DEFAULT_SHELL_MAX_DISPLAY_BYTES, DEFAULT_SHELL_MAX_OUTPUT_BYTES,
    DEFAULT_SHELL_MAX_OUTPUT_LINES, DEFAULT_SHELL_MAX_TIMEOUT_SECS, DEFAULT_SHELL_TIMEOUT_SECS,
    SHELL_TOOL_NAME, ShellTool,
};
pub use write::{WRITE_TOOL_NAME, WriteTool};

/// Stable business-error codes produced by the tools in this crate.
pub mod error_code {
    /// An argument is missing, mistyped, or otherwise unusable.
    pub const INVALID_ARGUMENTS: &str = "invalid_arguments";
    /// The resolved path is not inside the configured root directory.
    pub const OUTSIDE_ROOT: &str = "outside_root";
    /// The file or directory does not exist.
    pub const NOT_FOUND: &str = "not_found";
    /// The path exists but is not a regular file.
    pub const NOT_A_FILE: &str = "not_a_file";
    /// The path exists but is not a directory.
    pub const NOT_A_DIRECTORY: &str = "not_a_directory";
    /// The file content is not valid UTF-8 text.
    pub const NOT_UTF8: &str = "not_utf8";
    /// The file is binary or an image and cannot be read as text.
    pub const BINARY_FILE: &str = "binary_file";
    /// The glob pattern does not compile.
    pub const INVALID_GLOB: &str = "invalid_glob";
    /// The regular expression does not compile.
    pub const INVALID_REGEX: &str = "invalid_regex";
    /// The edit old_string was not found.
    pub const NO_MATCH: &str = "no_match";
    /// The edit old_string occurs more than once.
    pub const NOT_UNIQUE: &str = "not_unique";
    /// The shell command exceeded its time limit and was terminated.
    pub const TIMEOUT: &str = "timeout";
    /// The platform shell could not be started.
    pub const SPAWN_FAILED: &str = "spawn_failed";
    /// Any other filesystem I/O failure.
    pub const IO_ERROR: &str = "io_error";
}
