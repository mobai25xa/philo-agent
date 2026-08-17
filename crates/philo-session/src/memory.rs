//! Thread-safe in-memory implementation of the session store contract.
//!
//! A thin wrapper over the shared validation core: every commit is validated
//! and applied by [`SessionProjection`], so this backend cannot drift from
//! durable backends sharing the same rules.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::entry::{SessionCommit, SessionId, SessionTransaction};
use crate::error::SessionError;
use crate::projection::SessionProjection;
use crate::store::{SessionFuture, SessionStore};
use crate::view::SessionContextView;

/// Thread-safe in-memory implementation of [`SessionStore`].
#[derive(Debug, Default)]
pub struct MemorySessionStore {
    sessions: Mutex<HashMap<SessionId, SessionProjection>>,
}

impl MemorySessionStore {
    /// Creates an empty in-memory store.
    pub fn new() -> Self {
        Self::default()
    }
}

fn unavailable() -> SessionError {
    SessionError::store_unavailable("in-memory session store mutex poisoned")
}

impl SessionStore for MemorySessionStore {
    fn context_view<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> SessionFuture<'a, Result<SessionContextView, SessionError>> {
        Box::pin(async move {
            let sessions = self.sessions.lock().map_err(|_| unavailable())?;
            Ok(sessions
                .get(session_id)
                .map_or_else(SessionProjection::empty, Clone::clone)
                .context_view(session_id))
        })
    }

    fn commit<'a>(
        &'a self,
        transaction: SessionTransaction,
    ) -> SessionFuture<'a, Result<SessionCommit, SessionError>> {
        Box::pin(async move {
            let mut sessions = self.sessions.lock().map_err(|_| unavailable())?;
            let projection = sessions
                .get(&transaction.session_id)
                .cloned()
                .unwrap_or_else(SessionProjection::empty);
            if transaction.expected_revision != projection.revision() {
                return Err(SessionError::RevisionConflict {
                    expected: transaction.expected_revision,
                    actual: projection.revision(),
                });
            }
            let applied = projection.apply(&transaction)?;
            let commit = applied.commit();
            sessions.insert(transaction.session_id, applied.into_projection());
            Ok(commit)
        })
    }

    fn list_sessions(&self) -> SessionFuture<'_, Result<Vec<SessionId>, SessionError>> {
        Box::pin(async move {
            let sessions = self.sessions.lock().map_err(|_| unavailable())?;
            Ok(sessions.keys().cloned().collect())
        })
    }
}
