//! Validation errors and the asynchronous session store contract.

use std::future::Future;
use std::pin::Pin;

use crate::entry::{
    OperationId, SessionCommit, SessionId, SessionRevision, SessionTransaction, TurnId,
};
use crate::view::SessionContextView;

/// Stable validation failures for malformed transaction entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionValidationError {
    /// Transactions must append at least one entry.
    EmptyTransaction,
    /// Each new entry must extend the immediately preceding linear leaf.
    InvalidParent {
        /// Index of the malformed entry.
        entry_index: usize,
    },
    /// The requested current leaf must be the transaction's final entry.
    InvalidCurrentLeaf,
    /// An operation identifier was already started.
    DuplicateOperation { operation_id: OperationId },
    /// A turn identifier was already started.
    DuplicateTurn { turn_id: TurnId },
    /// An entry references an operation that is absent or no longer active.
    InvalidOperationReference { operation_id: OperationId },
    /// An entry references a turn that is absent or no longer active.
    InvalidTurnReference { turn_id: TurnId },
    /// A turn received more than one user message.
    DuplicateUserMessage { turn_id: TurnId },
    /// A user message payload is structurally malformed: empty parts,
    /// an empty text part, or an empty media type.
    InvalidUserMessage { turn_id: TurnId },
    /// A turn received more than one assistant message.
    DuplicateAssistantMessage { turn_id: TurnId },
    /// A turn outcome lacks, or conflicts with, its required message/failure.
    InvalidTurnOutcome { turn_id: TurnId },
    /// An operation outcome does not match its terminated turn.
    InvalidOperationOutcome { operation_id: OperationId },
    /// A tool batch is malformed or duplicated.
    InvalidToolBatch { turn_id: TurnId },
    /// A tool result does not match its durable batch.
    InvalidToolResult { turn_id: TurnId },
}

/// Session read or commit failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionError {
    /// The transaction expected a stale or future revision.
    RevisionConflict {
        /// Revision supplied by the transaction.
        expected: SessionRevision,
        /// Current durable revision.
        actual: SessionRevision,
    },
    /// Entry relationships or lifecycle facts were invalid.
    Validation(SessionValidationError),
    /// The backend storage is unavailable or an I/O operation failed.
    /// Carries stable diagnostic text, never a backend-private error object.
    StoreUnavailable {
        /// Stable, human-readable failure description.
        reason: String,
    },
}

impl SessionError {
    /// Creates a backend-unavailable failure with stable diagnostic text.
    pub fn store_unavailable(reason: impl Into<String>) -> Self {
        Self::StoreUnavailable {
            reason: reason.into(),
        }
    }
}

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
