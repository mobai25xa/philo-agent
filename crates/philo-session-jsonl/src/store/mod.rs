//! Public store orchestration, cached session state, and durable commit.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use philo_session::{
    SessionCommit, SessionContextView, SessionError, SessionFuture, SessionId, SessionProjection,
    SessionStore, SessionTransaction,
};

use crate::artifact::{ARTIFACTS_DIR, store_artifact};
use crate::error::{JsonlOpenError, io_error, io_error_text};
use crate::schema::{PendingArtifact, SCHEMA_VERSION, TransactionRecord, encode_entry};

mod layout;
mod recovery;

use layout::{LOG_FILE, acquire_lock, decode_session_dir_name, fsync_dir, session_dir_name};
pub use recovery::RecoveryReport;

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
        let state = recovery::recover_locked(&self.session_dir(session_id))?;
        let report = state.report.clone();
        sessions.insert(session_id.clone(), state);
        Ok(report)
    }

    fn session_dir(&self, session_id: &SessionId) -> PathBuf {
        self.root.join(session_dir_name(session_id))
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
                recovery::recover_locked(&self.session_dir(session_id))?
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

            let mut pending_artifacts: Vec<PendingArtifact> = Vec::new();
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
