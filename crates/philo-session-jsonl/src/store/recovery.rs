//! Transaction-boundary recovery, replay, torn-tail repair, and orphan
//! artifact reporting.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::path::Path;

use philo_session::SessionProjection;

use crate::artifact::{ARTIFACTS_DIR, load_artifact};
use crate::error::{JsonlOpenError, io_error};
use crate::schema::{SCHEMA_VERSION, TransactionRecord, decode_entry};

use super::SessionState;
use super::layout::{LOG_FILE, acquire_lock};

/// What recovery observed when a session was first touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    pub(super) transactions: u64,
    pub(super) truncated_tail_bytes: u64,
    pub(super) orphan_artifacts: Vec<String>,
}

impl RecoveryReport {
    /// Number of complete transactions rebuilt from the log.
    pub fn transactions(&self) -> u64 {
        self.transactions
    }

    /// Bytes of physically incomplete tail data truncated as crash residue.
    pub fn truncated_tail_bytes(&self) -> u64 {
        self.truncated_tail_bytes
    }

    /// Whether a torn tail was truncated during recovery.
    pub fn tail_was_truncated(&self) -> bool {
        self.truncated_tail_bytes > 0
    }

    /// Sorted artifact file names referenced by no replayed transaction.
    pub fn orphan_artifacts(&self) -> &[String] {
        &self.orphan_artifacts
    }
}

pub(super) fn recover_locked(dir: &Path) -> Result<SessionState, JsonlOpenError> {
    let lock = acquire_lock(dir)?;
    let log_path = dir.join(LOG_FILE);
    let bytes = match fs::read(&log_path) {
        Ok(bytes) => bytes,
        // A crash between directory and log creation leaves an empty session.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(io_error("reading session log", &error)),
    };

    let artifacts_dir = dir.join(ARTIFACTS_DIR);
    let mut referenced_artifacts = HashSet::new();
    let mut projection = SessionProjection::empty();
    let mut line_number: u64 = 0;
    let mut offset: usize = 0;
    let mut truncate_at: Option<u64> = None;
    while offset < bytes.len() {
        line_number += 1;
        let line_start = offset;
        let Some(relative_end) = bytes[offset..].iter().position(|byte| *byte == b'\n') else {
            truncate_at = Some(line_start as u64);
            break;
        };
        let line = &bytes[offset..offset + relative_end];
        offset += relative_end + 1;
        let is_last_line = offset >= bytes.len();
        // Peek `v` before the v2 record shape so a v1 line is
        // `UnsupportedSchema`, not a parse `Corrupt` from old fields.
        let value = match serde_json::from_slice::<serde_json::Value>(line) {
            Ok(value) => value,
            Err(error) if is_last_line && error.is_eof() => {
                truncate_at = Some(line_start as u64);
                break;
            }
            Err(error) => {
                return Err(JsonlOpenError::Corrupt {
                    line: line_number,
                    reason: format!("envelope parse failed: {error}"),
                });
            }
        };
        let found = match value.get("v").and_then(|version| version.as_u64()) {
            Some(version) => version,
            None => {
                return Err(JsonlOpenError::Corrupt {
                    line: line_number,
                    reason: "envelope parse failed: missing numeric schema version".to_owned(),
                });
            }
        };
        if found != SCHEMA_VERSION {
            return Err(JsonlOpenError::UnsupportedSchema { found });
        }
        let record = match serde_json::from_value::<TransactionRecord>(value) {
            Ok(record) => record,
            Err(error) => {
                return Err(JsonlOpenError::Corrupt {
                    line: line_number,
                    reason: format!("envelope parse failed: {error}"),
                });
            }
        };
        if record.v != SCHEMA_VERSION {
            return Err(JsonlOpenError::UnsupportedSchema { found: record.v });
        }
        if record.revision != line_number {
            return Err(JsonlOpenError::Corrupt {
                line: line_number,
                reason: format!(
                    "revision {} does not match transaction position {line_number}",
                    record.revision
                ),
            });
        }

        let mut load = |hash: &str, expected_len: u64| {
            let bytes = load_artifact(&artifacts_dir, hash, expected_len)?;
            referenced_artifacts.insert(hash.to_owned());
            Ok(bytes)
        };
        let mut entries = Vec::with_capacity(record.entries.len());
        for entry_record in record.entries {
            entries.push(decode_entry(entry_record, &mut load).map_err(|reason| {
                JsonlOpenError::Corrupt {
                    line: line_number,
                    reason,
                }
            })?);
        }
        projection
            .replay(&entries)
            .map_err(|error| JsonlOpenError::Corrupt {
                line: line_number,
                reason: format!("validation core rejected the transaction: {error:?}"),
            })?;
    }

    let mut orphan_artifacts = Vec::new();
    if artifacts_dir.is_dir() {
        let listing =
            fs::read_dir(&artifacts_dir).map_err(|error| io_error("listing artifacts", &error))?;
        for entry in listing {
            let entry = entry.map_err(|error| io_error("listing artifacts", &error))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !referenced_artifacts.contains(&name) {
                orphan_artifacts.push(name);
            }
        }
        orphan_artifacts.sort();
    }

    let mut truncated_tail_bytes = 0;
    if let Some(keep) = truncate_at {
        truncated_tail_bytes = bytes.len() as u64 - keep;
        let file = OpenOptions::new()
            .write(true)
            .open(&log_path)
            .map_err(|error| io_error("opening log for tail truncation", &error))?;
        file.set_len(keep)
            .map_err(|error| io_error("truncating torn tail", &error))?;
        file.sync_all()
            .map_err(|error| io_error("syncing truncated log", &error))?;
    }

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|error| io_error("opening log for append", &error))?;
    Ok(SessionState {
        report: RecoveryReport {
            transactions: projection.revision().get(),
            truncated_tail_bytes,
            orphan_artifacts,
        },
        projection,
        log,
        _lock: lock,
        poisoned: None,
    })
}
