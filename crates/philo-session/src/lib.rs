//! Append-only session contract, shared validation core, and in-memory store.

mod entry;
mod error;
mod memory;
mod projection;
mod store;
mod tool_entry;
mod view;

pub use entry::{
    CancelReason, EntryId, NewEntryParent, NewSessionEntry, OperationId, OperationOutcome,
    SessionAssistantBlock, SessionCommit, SessionEntry, SessionEntryKind, SessionId,
    SessionRevision, SessionTransaction, SessionUserPart, ToolBatchId, ToolCallId, TurnFailure,
    TurnFailureKind, TurnId, TurnOutcome,
};
pub use error::{SessionError, SessionValidationError};
pub use memory::MemorySessionStore;
pub use projection::{AppliedTransaction, SessionProjection};
pub use store::{SessionFuture, SessionStore};
pub use tool_entry::{SessionToolCall, SessionToolResult, ToolResultOutcome};
pub use view::{ContextMessage, OpenTurnInfo, SessionContextView, UnfilledBatch};
