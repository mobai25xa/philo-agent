//! Public store handle over a dedicated JSONL actor thread.

use std::fmt;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use philo_session::{
    SessionCommit, SessionContextView, SessionError, SessionFuture, SessionId, SessionProjection,
    SessionStore, SessionSummary, SessionTransaction,
};
use tokio::sync::oneshot;

use crate::error::{JsonlOpenError, io_error, io_error_text};

mod actor;
mod layout;
mod recovery;

use actor::{Reply, StoreActor, StoreCommand};
pub use recovery::RecoveryReport;

/// Bounded store-command queue capacity. Full enqueue returns [`SessionError::StoreBusy`].
pub const STORE_COMMAND_CAP: usize = 64;

pub(super) struct SessionState {
    projection: SessionProjection,
    log: File,
    /// Held for the lifetime of the state; the OS releases the advisory lock
    /// when the file handle closes (including process crash).
    _lock: File,
    report: RecoveryReport,
    /// Title bytes currently mirrored in the sidecar file. `None` means no
    /// file content is trusted; a successful sync stores the written value.
    sidecar_title: Option<String>,
    /// After a write/fsync failure the on-disk state is untrusted: refuse
    /// further commits until a fresh store instance re-opens and recovers.
    poisoned: Option<String>,
}

impl SessionState {
    /// Rewrites the title sidecar when the resolved title changed. Best
    /// effort: a failed write leaves the cache untouched so the next
    /// commit retries; listing falls back to ids meanwhile.
    pub(super) fn sync_title_sidecar(&mut self, dir: &Path) {
        let resolved = self.projection.title();
        if resolved == self.sidecar_title {
            return;
        }
        layout::write_title_file(dir, resolved.as_deref());
        if resolved.is_none() || layout::read_title_file(dir) == resolved {
            self.sidecar_title = resolved;
        }
    }
}

struct CommandHandle {
    root: PathBuf,
    tx: SyncSender<StoreCommand>,
    closed: AtomicBool,
}

impl Drop for CommandHandle {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        let (reply, _) = mpsc::channel();
        let _ = self.tx.try_send(StoreCommand::Shutdown { reply });
    }
}

struct ThreadSlot {
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for ThreadSlot {
    fn drop(&mut self) {
        if let Some(thread) = take_thread(&self.thread) {
            let _ = thread.join();
        }
    }
}

fn take_thread(thread: &Mutex<Option<JoinHandle<()>>>) -> Option<JoinHandle<()>> {
    thread
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
}

/// Durable [`SessionStore`] over per-session append-only JSONL logs.
///
/// Cloneable handle to a dedicated OS thread (`philo-jsonl-store`) that owns
/// session projections, file handles, and advisory locks. Async methods copy
/// inputs, `try_send` on a bounded channel, and await a oneshot reply.
#[derive(Clone)]
pub struct JsonlSessionStore {
    commands: Arc<CommandHandle>,
    thread: Arc<ThreadSlot>,
}

impl fmt::Debug for JsonlSessionStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsonlSessionStore")
            .field("root", &self.commands.root)
            .finish()
    }
}

impl JsonlSessionStore {
    /// Opens a store rooted at `root`, creating the directory if needed.
    /// Sessions are recovered lazily on first touch.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, JsonlOpenError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|error| io_error("creating store root", &error))?;
        let (tx, rx) = mpsc::sync_channel(STORE_COMMAND_CAP);
        let actor_root = root.clone();
        let thread = thread::Builder::new()
            .name("philo-jsonl-store".to_owned())
            .spawn(move || StoreActor::new(actor_root, rx).run())
            .map_err(|error| io_error("spawning jsonl store actor", &error))?;
        Ok(Self {
            commands: Arc::new(CommandHandle {
                root,
                tx,
                closed: AtomicBool::new(false),
            }),
            thread: Arc::new(ThreadSlot {
                thread: Mutex::new(Some(thread)),
            }),
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
        let (reply, rx) = mpsc::channel();
        self.try_enqueue_open(StoreCommand::RecoverSession {
            session_id: session_id.clone(),
            reply,
        })?;
        rx.recv()
            .map_err(|_| io_error_text("jsonl store actor stopped"))?
    }

    /// Enumerates the session ids present under the store root.
    ///
    /// Read-only: takes no session locks, triggers no recovery, and neither
    /// creates nor modifies any file — the disk is byte-for-byte unchanged
    /// afterwards. Directory entries that are not canonical session-dir
    /// encodings (internal files, foreign directories) are skipped silently.
    /// Order is not specified; callers sort as needed.
    pub fn list_sessions(&self) -> Result<Vec<SessionId>, JsonlOpenError> {
        let (reply, rx) = mpsc::channel();
        self.try_enqueue_open(StoreCommand::ListSessions {
            reply: Reply::Sync(reply),
        })?;
        rx.recv()
            .map_err(|_| io_error_text("jsonl store actor stopped"))?
    }

    /// Enumerates sessions with their cached display titles.
    ///
    /// Same read-only contract as [`JsonlSessionStore::list_sessions`]:
    /// titles come from a per-session sidecar written under lock by the
    /// actor; sessions without one report `None` and callers fall back
    /// to the id.
    pub fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, JsonlOpenError> {
        let (reply, rx) = mpsc::channel();
        self.try_enqueue_open(StoreCommand::ListSessionSummaries {
            reply: Reply::Sync(reply),
        })?;
        rx.recv()
            .map_err(|_| io_error_text("jsonl store actor stopped"))?
    }

    /// Drains already-queued commits, rejects new requests, fsyncs, releases
    /// locks, and joins the actor thread.
    pub fn shutdown(&self) -> Result<(), JsonlOpenError> {
        self.commands.closed.store(true, Ordering::Release);
        let (reply, rx) = mpsc::channel();
        match self.commands.tx.send(StoreCommand::Shutdown { reply }) {
            Ok(()) => {}
            Err(_) => {
                self.join_actor();
                return Err(io_error_text("jsonl store actor is unavailable"));
            }
        }
        let _ = rx.recv();
        self.join_actor();
        Ok(())
    }

    fn join_actor(&self) {
        if let Some(thread) = take_thread(&self.thread.thread) {
            let _ = thread.join();
        }
    }

    fn try_enqueue(&self, command: StoreCommand) -> Result<(), SessionError> {
        if self.commands.closed.load(Ordering::Acquire) {
            return Err(SessionError::store_unavailable("jsonl store is shut down"));
        }
        match self.commands.tx.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(SessionError::store_busy(
                "jsonl store command queue is full",
            )),
            Err(TrySendError::Disconnected(_)) => Err(SessionError::store_unavailable(
                "jsonl store actor is unavailable",
            )),
        }
    }

    fn try_enqueue_open(&self, command: StoreCommand) -> Result<(), JsonlOpenError> {
        if self.commands.closed.load(Ordering::Acquire) {
            return Err(io_error_text("jsonl store is shut down"));
        }
        match self.commands.tx.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(io_error_text("jsonl store command queue is full")),
            Err(TrySendError::Disconnected(_)) => {
                Err(io_error_text("jsonl store actor is unavailable"))
            }
        }
    }

    fn await_store<T: Send + 'static>(
        &self,
        command: StoreCommand,
        rx: oneshot::Receiver<Result<T, SessionError>>,
    ) -> SessionFuture<'_, Result<T, SessionError>> {
        match self.try_enqueue(command) {
            Ok(()) => Box::pin(async move {
                rx.await
                    .map_err(|_| SessionError::store_unavailable("jsonl store actor stopped"))?
            }),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }

    #[cfg(test)]
    fn block_actor_for_test(
        &self,
    ) -> (
        oneshot::Receiver<()>,
        mpsc::Sender<()>,
        oneshot::Receiver<()>,
    ) {
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands
            .tx
            .send(StoreCommand::Block {
                started: started_tx,
                release: release_rx,
                reply: reply_tx,
            })
            .expect("enqueue block command");
        (started_rx, release_tx, reply_rx)
    }
}

impl SessionStore for JsonlSessionStore {
    fn context_view<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> SessionFuture<'a, Result<SessionContextView, SessionError>> {
        let (reply, rx) = oneshot::channel();
        self.await_store(
            StoreCommand::ContextView {
                session_id: session_id.clone(),
                reply,
            },
            rx,
        )
    }

    fn commit<'a>(
        &'a self,
        transaction: SessionTransaction,
    ) -> SessionFuture<'a, Result<SessionCommit, SessionError>> {
        let (reply, rx) = oneshot::channel();
        self.await_store(StoreCommand::Commit { transaction, reply }, rx)
    }

    fn list_sessions(&self) -> SessionFuture<'_, Result<Vec<SessionId>, SessionError>> {
        let (reply, rx) = oneshot::channel();
        match self.try_enqueue(StoreCommand::ListSessions {
            reply: Reply::Async(reply),
        }) {
            Ok(()) => Box::pin(async move {
                match rx.await {
                    Ok(Ok(ids)) => Ok(ids),
                    Ok(Err(error)) => Err(SessionError::store_unavailable(error.to_string())),
                    Err(_) => Err(SessionError::store_unavailable("jsonl store actor stopped")),
                }
            }),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }

    fn list_session_summaries(
        &self,
    ) -> SessionFuture<'_, Result<Vec<SessionSummary>, SessionError>> {
        let (reply, rx) = oneshot::channel();
        match self.try_enqueue(StoreCommand::ListSessionSummaries {
            reply: Reply::Async(reply),
        }) {
            Ok(()) => Box::pin(async move {
                match rx.await {
                    Ok(Ok(summaries)) => Ok(summaries),
                    Ok(Err(error)) => Err(SessionError::store_unavailable(error.to_string())),
                    Err(_) => Err(SessionError::store_unavailable("jsonl store actor stopped")),
                }
            }),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use philo_session::{SessionError, SessionId, SessionStore};

    use super::{JsonlSessionStore, STORE_COMMAND_CAP};

    struct TempRoot {
        path: std::path::PathBuf,
    }

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "philo-session-jsonl-actor-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("create temp root");
            Self { path }
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    struct ReleaseOnDrop(Option<std::sync::mpsc::Sender<()>>);

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn full_command_queue_returns_store_busy() {
        let root = TempRoot::new();
        let store = JsonlSessionStore::open(&root.path).expect("open");
        let (started, release, _done) = store.block_actor_for_test();
        let _release = ReleaseOnDrop(Some(release.clone()));
        started.await.expect("actor entered block");

        let session_id = SessionId::new("busy");
        let mut queued = Vec::with_capacity(STORE_COMMAND_CAP);
        for _ in 0..STORE_COMMAND_CAP {
            queued.push(SessionStore::context_view(&store, &session_id));
        }
        let error = SessionStore::context_view(&store, &session_id)
            .await
            .expect_err("queue full");
        assert!(
            matches!(error, SessionError::StoreBusy { .. }),
            "expected StoreBusy, got {error:?}"
        );

        release.send(()).expect("release actor");
        for future in queued {
            future.await.expect("queued view completes");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_rejects_subsequent_requests() {
        let root = TempRoot::new();
        let store = JsonlSessionStore::open(&root.path).expect("open");
        store.shutdown().expect("shutdown");
        let error = SessionStore::context_view(&store, &SessionId::new("ghost"))
            .await
            .expect_err("shut down");
        assert!(
            matches!(error, SessionError::StoreUnavailable { .. }),
            "expected StoreUnavailable, got {error:?}"
        );
    }
}
