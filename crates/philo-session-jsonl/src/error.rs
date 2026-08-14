//! Public open/recovery errors and internal I/O error normalization.

use std::fmt;
use std::path::PathBuf;

/// Why a session (or the store root) could not be opened.
#[derive(Debug)]
pub enum JsonlOpenError {
    /// Filesystem failure with a redacted description.
    Io { context: String },
    /// Another writer holds the session's advisory lock.
    Locked { path: PathBuf },
    /// A complete log line failed to parse or validate.
    Corrupt { line: u64, reason: String },
    /// The envelope schema version is not readable by this crate.
    UnsupportedSchema { found: u64 },
}

impl fmt::Display for JsonlOpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context } => write!(f, "jsonl io failure: {context}"),
            Self::Locked { path } => {
                write!(
                    f,
                    "jsonl session locked by another writer: {}",
                    path.display()
                )
            }
            Self::Corrupt { line, reason } => {
                write!(f, "jsonl log corrupt at line {line}: {reason}")
            }
            Self::UnsupportedSchema { found } => {
                write!(
                    f,
                    "jsonl schema version {found} is unsupported (expected 1)"
                )
            }
        }
    }
}

impl std::error::Error for JsonlOpenError {}

pub(crate) fn io_error(context: &str, error: &std::io::Error) -> JsonlOpenError {
    JsonlOpenError::Io {
        context: format!("{context} ({:?})", error.kind()),
    }
}

pub(crate) fn io_error_text(context: &str) -> JsonlOpenError {
    JsonlOpenError::Io {
        context: context.to_owned(),
    }
}
