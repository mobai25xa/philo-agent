//! Asynchronous session store contract.

use std::future::Future;
use std::pin::Pin;

use crate::entry::{SessionCommit, SessionId, SessionTransaction};
use crate::error::SessionError;
use crate::view::SessionContextView;

/// Boxed future returned by the object-safe session store contract.
pub type SessionFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One listed session: its identity plus a best-effort display title.
/// Titles are advisory presentation hints; readers fall back to the id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    /// The durable session identifier.
    pub session_id: SessionId,
    /// Resolved title, when the backend knows one.
    pub title: Option<String>,
}

impl SessionSummary {
    /// Creates a summary without a title.
    pub fn untitled(session_id: SessionId) -> Self {
        Self {
            session_id,
            title: None,
        }
    }
}

/// Asynchronous contract for reading and atomically committing session state.
pub trait SessionStore: Send + Sync {
    /// Reads a stable model context view. Unknown sessions are empty at revision zero.
    fn context_view<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> SessionFuture<'a, Result<SessionContextView, SessionError>>;

    /// Atomically appends a validated transaction.
    fn commit<'a>(
        &'a self,
        transaction: SessionTransaction,
    ) -> SessionFuture<'a, Result<SessionCommit, SessionError>>;

    /// Lists known session identifiers. Order is not specified.
    fn list_sessions(&self) -> SessionFuture<'_, Result<Vec<SessionId>, SessionError>>;

    /// Lists known sessions with display titles. Same enumeration contract
    /// as [`SessionStore::list_sessions`]: read-only, order unspecified.
    /// Backends that cannot resolve titles cheaply return `None` titles.
    fn list_session_summaries(
        &self,
    ) -> SessionFuture<'_, Result<Vec<SessionSummary>, SessionError>> {
        Box::pin(async {
            let ids = self.list_sessions().await?;
            Ok(ids.into_iter().map(SessionSummary::untitled).collect())
        })
    }
}
