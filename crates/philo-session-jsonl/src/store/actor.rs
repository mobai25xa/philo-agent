//! Dedicated OS-thread store actor. Owns session state, files, and locks.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};

use philo_session::{
    SessionCommit, SessionContextView, SessionError, SessionId, SessionProjection,
    SessionTransaction,
};
use tokio::sync::oneshot;

use crate::artifact::{ARTIFACTS_DIR, store_artifact};
use crate::error::{JsonlOpenError, io_error, io_error_text};
use crate::schema::{PendingArtifact, SCHEMA_VERSION, TransactionRecord, encode_entry};

use super::SessionState;
use super::layout::{LOG_FILE, acquire_lock, decode_session_dir_name, fsync_dir, session_dir_name};
use super::recovery::{self, RecoveryReport};

pub(super) enum Reply<T> {
    Async(oneshot::Sender<T>),
    Sync(std::sync::mpsc::Sender<T>),
}

impl<T> Reply<T> {
    pub(super) fn send(self, value: T) {
        match self {
            Self::Async(tx) => {
                let _ = tx.send(value);
            }
            Self::Sync(tx) => {
                let _ = tx.send(value);
            }
        }
    }
}

pub(super) enum StoreCommand {
    ContextView {
        session_id: SessionId,
        reply: oneshot::Sender<Result<SessionContextView, SessionError>>,
    },
    Commit {
        transaction: SessionTransaction,
        reply: oneshot::Sender<Result<SessionCommit, SessionError>>,
    },
    ListSessions {
        reply: Reply<Result<Vec<SessionId>, JsonlOpenError>>,
    },
    RecoverSession {
        session_id: SessionId,
        reply: std::sync::mpsc::Sender<Result<RecoveryReport, JsonlOpenError>>,
    },
    Shutdown {
        reply: std::sync::mpsc::Sender<()>,
    },
    #[cfg(test)]
    Block {
        started: oneshot::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
        reply: oneshot::Sender<()>,
    },
}

pub(super) struct StoreActor {
    root: PathBuf,
    sessions: HashMap<SessionId, SessionState>,
    rx: Receiver<StoreCommand>,
}

impl StoreActor {
    pub(super) fn new(root: PathBuf, rx: Receiver<StoreCommand>) -> Self {
        Self {
            root,
            sessions: HashMap::new(),
            rx,
        }
    }

    pub(super) fn run(mut self) {
        loop {
            match self.rx.recv() {
                Ok(command) => {
                    if self.dispatch(command) {
                        return;
                    }
                }
                Err(_) => {
                    self.finish();
                    return;
                }
            }
        }
    }

    /// Returns `true` when the actor should stop.
    fn dispatch(&mut self, command: StoreCommand) -> bool {
        match command {
            StoreCommand::ContextView { session_id, reply } => {
                let _ = reply.send(self.context_view(&session_id));
                false
            }
            StoreCommand::Commit { transaction, reply } => {
                let _ = reply.send(self.commit(transaction));
                false
            }
            StoreCommand::ListSessions { reply } => {
                reply.send(self.list_sessions());
                false
            }
            StoreCommand::RecoverSession { session_id, reply } => {
                let _ = reply.send(self.recover_session(&session_id));
                false
            }
            StoreCommand::Shutdown { reply } => {
                self.drain_queued_commits();
                self.finish();
                let _ = reply.send(());
                true
            }
            #[cfg(test)]
            StoreCommand::Block {
                started,
                release,
                reply,
            } => {
                let _ = started.send(());
                let _ = release.recv();
                let _ = reply.send(());
                false
            }
        }
    }

    fn drain_queued_commits(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(StoreCommand::Commit { transaction, reply }) => {
                    let _ = reply.send(self.commit(transaction));
                }
                Ok(StoreCommand::Shutdown { reply }) => {
                    let _ = reply.send(());
                }
                Ok(StoreCommand::ContextView { reply, .. }) => {
                    let _ = reply.send(Err(SessionError::store_unavailable(
                        "jsonl store shutting down",
                    )));
                }
                Ok(StoreCommand::ListSessions { reply }) => {
                    reply.send(Err(io_error_text("jsonl store shutting down")));
                }
                Ok(StoreCommand::RecoverSession { reply, .. }) => {
                    let _ = reply.send(Err(io_error_text("jsonl store shutting down")));
                }
                #[cfg(test)]
                Ok(StoreCommand::Block {
                    started,
                    release,
                    reply,
                }) => {
                    let _ = started.send(());
                    drop(release);
                    let _ = reply.send(());
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    fn finish(&mut self) {
        for state in self.sessions.values_mut() {
            let _ = state.log.sync_all();
        }
        self.sessions.clear();
    }

    fn session_dir(&self, session_id: &SessionId) -> PathBuf {
        self.root.join(session_dir_name(session_id))
    }

    fn context_view(&mut self, session_id: &SessionId) -> Result<SessionContextView, SessionError> {
        match self.touch(session_id, false) {
            Ok(Some(state)) => Ok(state.projection.context_view(session_id)),
            Ok(None) => Ok(SessionProjection::empty().context_view(session_id)),
            Err(error) => Err(SessionError::store_unavailable(error.to_string())),
        }
    }

    fn recover_session(
        &mut self,
        session_id: &SessionId,
    ) -> Result<RecoveryReport, JsonlOpenError> {
        if let Some(state) = self.sessions.get(session_id) {
            return Ok(state.report.clone());
        }
        if !self.session_dir(session_id).is_dir() {
            return Ok(RecoveryReport::empty());
        }
        let state = recovery::recover_locked(&self.session_dir(session_id))?;
        let report = state.report.clone();
        self.sessions.insert(session_id.clone(), state);
        Ok(report)
    }

    /// Read-only: takes no session locks, triggers no recovery, and neither
    /// creates nor modifies any file.
    fn list_sessions(&self) -> Result<Vec<SessionId>, JsonlOpenError> {
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
            report: RecoveryReport::empty(),
            poisoned: None,
        })
    }

    fn touch(
        &mut self,
        session_id: &SessionId,
        create_missing: bool,
    ) -> Result<Option<&mut SessionState>, JsonlOpenError> {
        if !self.sessions.contains_key(session_id) {
            let state = if self.session_dir(session_id).is_dir() {
                recovery::recover_locked(&self.session_dir(session_id))?
            } else if create_missing {
                self.create_session(session_id)?
            } else {
                return Ok(None);
            };
            self.sessions.insert(session_id.clone(), state);
        }
        Ok(Some(
            self.sessions
                .get_mut(session_id)
                .expect("state inserted or present"),
        ))
    }

    fn commit(&mut self, transaction: SessionTransaction) -> Result<SessionCommit, SessionError> {
        let session_id = transaction.session_id().clone();
        let artifacts_dir = self.session_dir(&session_id).join(ARTIFACTS_DIR);
        let state = self
            .touch(&session_id, true)
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
            for artifact in &pending_artifacts {
                if let Err(error) = store_artifact(&artifacts_dir, &artifact.hash, &artifact.bytes)
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
    }
}
