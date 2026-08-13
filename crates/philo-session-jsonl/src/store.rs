//! The JSONL session store: layout, locking, commit, and recovery.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use philo_session::{
    SessionCommit, SessionContextView, SessionError, SessionFuture, SessionId, SessionProjection,
    SessionStore, SessionTransaction,
};

use crate::artifact::{ARTIFACTS_DIR, load_artifact, store_artifact};
use crate::schema::{SCHEMA_VERSION, TransactionRecord, decode_entry, encode_entry};

const LOG_FILE: &str = "log.jsonl";
const LOCK_FILE: &str = "lock";

/// Why a session (or the store root) could not be opened.
#[derive(Debug)]
pub enum JsonlOpenError {
    /// Filesystem failure with a redacted description.
    Io { context: String },
    /// Another writer holds the session's advisory lock.
    Locked { path: PathBuf },
    /// A complete log line failed to parse or validate while later
    /// transactions still follow it; never auto-repaired.
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

/// What recovery observed when a session was first touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    transactions: u64,
    truncated_tail_bytes: u64,
    orphan_artifacts: Vec<String>,
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

    /// File names in `artifacts/` referenced by no replayed transaction:
    /// legal crash residue, tolerated and never deleted (sorted).
    pub fn orphan_artifacts(&self) -> &[String] {
        &self.orphan_artifacts
    }
}

struct SessionState {
    projection: SessionProjection,
    log: File,
    /// Held for the lifetime of the state; the OS releases the advisory lock
    /// when the file handle closes (including process crash).
    _lock: File,
    report: RecoveryReport,
    /// After a write/fsync failure the on-disk state is untrusted: refuse
    /// further commits until a fresh store instance re-opens and recovers.
    poisoned: Option<String>,
}

/// Durable [`SessionStore`] over per-session append-only JSONL logs.
pub struct JsonlSessionStore {
    root: PathBuf,
    sessions: Mutex<HashMap<SessionId, SessionState>>,
}

impl fmt::Debug for JsonlSessionStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsonlSessionStore")
            .field("root", &self.root)
            .finish()
    }
}

impl JsonlSessionStore {
    /// Opens a store rooted at `root`, creating the directory if needed.
    /// Sessions are recovered lazily on first touch.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, JsonlOpenError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|error| io_error("creating store root", &error))?;
        Ok(Self {
            root,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// Explicitly recovers one session and returns what recovery observed.
    ///
    /// Sessions without a directory report zero transactions and are not
    /// created. A session already recovered by this instance returns the
    /// report captured at first touch.
    pub fn recover_session(
        &self,
        session_id: &SessionId,
    ) -> Result<RecoveryReport, JsonlOpenError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| io_error_text("store mutex poisoned"))?;
        if let Some(state) = sessions.get(session_id) {
            return Ok(state.report.clone());
        }
        if !self.session_dir(session_id).is_dir() {
            return Ok(RecoveryReport {
                transactions: 0,
                truncated_tail_bytes: 0,
                orphan_artifacts: Vec::new(),
            });
        }
        let state = self.recover_locked(session_id)?;
        let report = state.report.clone();
        sessions.insert(session_id.clone(), state);
        Ok(report)
    }

    fn session_dir(&self, session_id: &SessionId) -> PathBuf {
        self.root.join(session_dir_name(session_id))
    }

    /// Locks and rebuilds one existing session directory.
    fn recover_locked(&self, session_id: &SessionId) -> Result<SessionState, JsonlOpenError> {
        let dir = self.session_dir(session_id);
        let lock = acquire_lock(&dir)?;
        let log_path = dir.join(LOG_FILE);
        let bytes = match fs::read(&log_path) {
            Ok(bytes) => bytes,
            // A crash between directory and log creation leaves an empty session.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(io_error("reading session log", &error)),
        };

        let artifacts_dir = dir.join(ARTIFACTS_DIR);
        let mut referenced_artifacts = std::collections::HashSet::new();
        let mut projection = SessionProjection::empty();
        let mut line_number: u64 = 0;
        let mut offset: usize = 0;
        let mut truncate_at: Option<u64> = None;
        while offset < bytes.len() {
            line_number += 1;
            let line_start = offset;
            let Some(relative_end) = bytes[offset..].iter().position(|byte| *byte == b'\n') else {
                // Physically incomplete tail: no line terminator.
                truncate_at = Some(line_start as u64);
                break;
            };
            let line = &bytes[offset..offset + relative_end];
            offset += relative_end + 1;
            let is_last_line = offset >= bytes.len();
            let record = match serde_json::from_slice::<TransactionRecord>(line) {
                Ok(record) => record,
                Err(error) if is_last_line && error.is_eof() => {
                    // Physically incomplete tail: the JSON itself is cut off.
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
            // Referenced artifacts are re-verified (hash and length) while
            // decoding: a missing or mismatching one is data corruption.
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

        // Artifact files never referenced by a replayed transaction are
        // legal crash residue: reported, tolerated, never deleted.
        let mut orphan_artifacts = Vec::new();
        if artifacts_dir.is_dir() {
            let listing = fs::read_dir(&artifacts_dir)
                .map_err(|error| io_error("listing artifacts", &error))?;
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

    /// Creates the directory, lock, and empty log for a brand-new session.
    fn create_session(&self, session_id: &SessionId) -> Result<SessionState, JsonlOpenError> {
        let dir = self.session_dir(session_id);
        fs::create_dir_all(&dir).map_err(|error| io_error("creating session dir", &error))?;
        let lock = acquire_lock(&dir)?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(LOG_FILE))
            .map_err(|error| io_error("creating session log", &error))?;
        // Make the directory entries themselves durable before the first
        // commit reports success.
        fsync_dir(&dir)?;
        fsync_dir(&self.root)?;
        Ok(SessionState {
            projection: SessionProjection::empty(),
            log,
            _lock: lock,
            report: RecoveryReport {
                transactions: 0,
                truncated_tail_bytes: 0,
                orphan_artifacts: Vec::new(),
            },
            poisoned: None,
        })
    }

    /// Enumerates the session ids present under the store root.
    ///
    /// Read-only: takes no session locks, triggers no recovery, and neither
    /// creates nor modifies any file — the disk is byte-for-byte unchanged
    /// afterwards. Directory entries that are not canonical session-dir
    /// encodings (internal files, foreign directories) are skipped silently.
    /// Order is not specified; callers sort as needed.
    pub fn list_sessions(&self) -> Result<Vec<SessionId>, JsonlOpenError> {
        let listing =
            fs::read_dir(&self.root).map_err(|error| io_error("listing store root", &error))?;
        let mut sessions = Vec::new();
        for entry in listing {
            let entry = entry.map_err(|error| io_error("listing store root", &error))?;
            let is_directory = entry
                .file_type()
                .map(|file_type| file_type.is_dir())
                .unwrap_or(false);
            if !is_directory {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Some(session_id) = decode_session_dir_name(name) {
                sessions.push(session_id);
            }
        }
        Ok(sessions)
    }

    /// Returns the recovered state for a session, recovering or (optionally)
    /// creating it on first touch. `None` means the session does not exist
    /// and creation was not requested.
    fn touch<'a>(
        &self,
        sessions: &'a mut HashMap<SessionId, SessionState>,
        session_id: &SessionId,
        create_missing: bool,
    ) -> Result<Option<&'a mut SessionState>, JsonlOpenError> {
        if !sessions.contains_key(session_id) {
            let state = if self.session_dir(session_id).is_dir() {
                self.recover_locked(session_id)?
            } else if create_missing {
                self.create_session(session_id)?
            } else {
                return Ok(None);
            };
            sessions.insert(session_id.clone(), state);
        }
        Ok(Some(
            sessions
                .get_mut(session_id)
                .expect("state inserted or present"),
        ))
    }
}

impl SessionStore for JsonlSessionStore {
    fn context_view<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> SessionFuture<'a, Result<SessionContextView, SessionError>> {
        Box::pin(async move {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| SessionError::store_unavailable("jsonl store mutex poisoned"))?;
            match self.touch(&mut sessions, session_id, false) {
                Ok(Some(state)) => Ok(state.projection.context_view(session_id)),
                Ok(None) => Ok(SessionProjection::empty().context_view(session_id)),
                Err(error) => Err(SessionError::store_unavailable(error.to_string())),
            }
        })
    }

    fn commit<'a>(
        &'a self,
        transaction: SessionTransaction,
    ) -> SessionFuture<'a, Result<SessionCommit, SessionError>> {
        Box::pin(async move {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| SessionError::store_unavailable("jsonl store mutex poisoned"))?;
            let state = self
                .touch(&mut sessions, transaction.session_id(), true)
                .map_err(|error| SessionError::store_unavailable(error.to_string()))?
                .expect("commit always creates missing sessions");
            if let Some(reason) = &state.poisoned {
                return Err(SessionError::store_unavailable(format!(
                    "session refused after write failure: {reason}"
                )));
            }
            if transaction.expected_revision() != state.projection.revision() {
                return Err(SessionError::RevisionConflict {
                    expected: transaction.expected_revision(),
                    actual: state.projection.revision(),
                });
            }
            let applied = state.projection.apply(&transaction)?;

            let mut pending_artifacts = Vec::new();
            let record = TransactionRecord {
                v: SCHEMA_VERSION,
                revision: applied.projection().revision().get(),
                entries: applied
                    .entries()
                    .iter()
                    .map(|entry| encode_entry(entry, &mut pending_artifacts))
                    .collect(),
            };
            // Barrier extension (ADR-0002): every artifact newly referenced
            // by this transaction is durable before the log line appends, so
            // a visible reference always points at a complete fsynced file.
            // Content addressing makes re-submitting the same image a no-op.
            if !pending_artifacts.is_empty() {
                let artifacts_dir = self
                    .session_dir(transaction.session_id())
                    .join(ARTIFACTS_DIR);
                for artifact in &pending_artifacts {
                    if let Err(error) =
                        store_artifact(&artifacts_dir, &artifact.hash, &artifact.bytes)
                    {
                        let reason = format!("artifact write or fsync failed ({:?})", error.kind());
                        state.poisoned = Some(reason.clone());
                        return Err(SessionError::store_unavailable(reason));
                    }
                }
                if let Err(error) = fsync_dir(&artifacts_dir) {
                    let reason = format!("artifact directory sync failed: {error}");
                    state.poisoned = Some(reason.clone());
                    return Err(SessionError::store_unavailable(reason));
                }
            }
            let mut line = serde_json::to_vec(&record).map_err(|error| {
                SessionError::store_unavailable(format!("envelope serialization failed: {error}"))
            })?;
            line.push(b'\n');
            let written = state
                .log
                .write_all(&line)
                .and_then(|()| state.log.sync_all());
            if let Err(error) = written {
                let reason = format!("append or fsync failed ({:?})", error.kind());
                state.poisoned = Some(reason.clone());
                return Err(SessionError::store_unavailable(reason));
            }

            let commit = applied.commit();
            state.projection = applied.into_projection();
            Ok(commit)
        })
    }
}

/// Deterministic, reversible, collision-free directory encoding for a
/// session id. Lowercase ASCII letters, digits, `-`, and `_` pass through;
/// every other byte (including uppercase, since common filesystems are
/// case-insensitive) becomes `%XX`. The fixed `s-` prefix keeps names clear
/// of platform-reserved words. Pinned by the golden format tests.
fn session_dir_name(session_id: &SessionId) -> String {
    let mut name = String::from("s-");
    for byte in session_id.as_str().bytes() {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => name.push(char::from(byte)),
            other => {
                name.push('%');
                name.push_str(&format!("{other:02X}"));
            }
        }
    }
    name
}

/// Inverse of [`session_dir_name`]: decodes a directory name back to the
/// session id it encodes. Returns `None` for anything that is not a
/// canonical encoding — wrong prefix, malformed escape, non-UTF-8 id bytes,
/// or a non-canonical form that would not re-encode to the same name.
fn decode_session_dir_name(name: &str) -> Option<SessionId> {
    let encoded = name.strip_prefix("s-")?;
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let high = hex_value(*bytes.get(index + 1)?)?;
                let low = hex_value(*bytes.get(index + 2)?)?;
                decoded.push((high << 4) | low);
                index += 3;
            }
            byte @ (b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_') => {
                decoded.push(byte);
                index += 1;
            }
            _ => return None,
        }
    }
    let session_id = SessionId::new(String::from_utf8(decoded).ok()?);
    (session_dir_name(&session_id) == name).then_some(session_id)
}

/// The encoder writes uppercase hex only; lowercase forms are non-canonical.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Takes the session's OS advisory lock; released automatically when the
/// returned handle closes, including on process crash.
fn acquire_lock(dir: &Path) -> Result<File, JsonlOpenError> {
    let path = dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| io_error("opening lock file", &error))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(JsonlOpenError::Locked { path }),
        Err(TryLockError::Error(error)) => Err(io_error("acquiring session lock", &error)),
    }
}

#[cfg(unix)]
fn fsync_dir(path: &Path) -> Result<(), JsonlOpenError> {
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(|error| io_error("syncing directory", &error))
}

/// Windows has no user-mode directory fsync; NTFS journals metadata itself.
#[cfg(not(unix))]
fn fsync_dir(_path: &Path) -> Result<(), JsonlOpenError> {
    Ok(())
}

fn io_error(context: &str, error: &std::io::Error) -> JsonlOpenError {
    JsonlOpenError::Io {
        context: format!("{context} ({:?})", error.kind()),
    }
}

fn io_error_text(context: &str) -> JsonlOpenError {
    JsonlOpenError::Io {
        context: context.to_owned(),
    }
}
