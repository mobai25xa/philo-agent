//! Asynchronous session store contract.

use std::future::Future;
use std::pin::Pin;

use crate::entry::{SessionCommit, SessionId, SessionTransaction};
use crate::error::SessionError;
use crate::view::SessionContextView;

/// Boxed future returned by the object-safe session store contract.
pub type SessionFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

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
}
