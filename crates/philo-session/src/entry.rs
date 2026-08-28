//! Durable identifiers, entry payloads, transactions and commit results.

use crate::generation_choice::SessionGenerationChoice;
use crate::tool_entry::{SessionToolCall, SessionToolResult};
use crate::usage::SessionTokenUsage;

macro_rules! string_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        pub struct $name(String);

        impl $name {
            #[doc = concat!("Creates a new `", stringify!($name), "`.")]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            #[doc = concat!("Returns the `", stringify!($name), "` value.")]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

string_id!(SessionId, "Identifies one append-only session.");
string_id!(EntryId, "Identifies a committed entry within a session.");
string_id!(OperationId, "Identifies one runtime operation.");
string_id!(TurnId, "Identifies one durable turn.");
string_id!(ToolBatchId, "Identifies one durable tool batch.");
string_id!(ToolCallId, "Identifies one durable tool call.");

/// Monotonic optimistic-concurrency revision of a session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionRevision(u64);

impl SessionRevision {
    /// The revision of a session with no committed transactions.
    pub const ZERO: Self = Self(0);

    /// Creates a revision from its numeric representation.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric representation.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns the next revision after one successful transaction.
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// Why a turn or operation was cancelled. The reason is part of the durable
/// fact; runtimes inject it when mapping terminal facts into transactions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelReason {
    /// The user requested orderly cancellation.
    User,
    /// A runtime operation timeout triggered automatic cancellation.
    Timeout,
    /// An unfinished turn left behind by an interrupted process was sealed
    /// before the next turn started.
    Abandoned,
}

/// Terminal outcome stored for a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The turn committed a final assistant message.
    Succeeded,
    /// The turn committed a normalized failure.
    Failed,
    /// The turn was orderly terminated; the reason records why.
    Cancelled {
        /// The durable cancellation reason.
        reason: CancelReason,
    },
}

/// Terminal outcome stored for an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationOutcome {
    /// The operation durably completed its turn.
    Succeeded,
    /// The operation durably recorded a failed turn.
    Failed,
    /// The operation durably recorded a cancelled turn; the reason must
    /// match the turn's within one transaction.
    Cancelled {
        /// The durable cancellation reason.
        reason: CancelReason,
    },
}

/// Stable category for a durable turn failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnFailureKind {
    /// The logical model call failed.
    ModelCall,
    /// The model returned invalid semantic output.
    InvalidModelOutput,
    /// A required persistence operation failed.
    Persistence,
    /// Runtime orchestration failed.
    RuntimeDriver,
    /// A tool infrastructure or execution failure.
    ToolExecution,
}

/// Normalized durable description of an unrecoverable turn failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnFailure {
    kind: TurnFailureKind,
    message: String,
}

impl TurnFailure {
    /// Creates a normalized failure description.
    pub fn new(kind: TurnFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Returns the stable failure category.
    pub fn kind(&self) -> TurnFailureKind {
        self.kind
    }

    /// Returns diagnostic text without store- or provider-specific error values.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// One durable part of a multi-part user message. The session is the
/// persistent source of truth for image bytes; it never interprets,
/// validates, or transcodes media content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionUserPart {
    Text(String),
    Image { media_type: String, bytes: Vec<u8> },
}

impl SessionUserPart {
    /// Convenience for the common single-text-part payload.
    pub fn text_parts(text: impl Into<String>) -> Vec<SessionUserPart> {
        vec![SessionUserPart::Text(text.into())]
    }
}

/// One ordered block of assistant output. Tool batches and final messages
/// share this type: a batch must contain at least one [`Self::ToolCall`],
/// while a final message may contain only [`Self::Text`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionAssistantBlock {
    /// Non-empty assistant text, in source order among neighboring blocks.
    Text { text: String },
    /// A model-requested tool call.
    ToolCall(SessionToolCall),
}

/// The durable payload of a session entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEntryKind {
    /// Records the beginning of an operation.
    OperationStarted { operation_id: OperationId },
    /// Records the turn owned by an operation.
    TurnStarted {
        operation_id: OperationId,
        turn_id: TurnId,
    },
    /// Records the single user message of a turn.
    UserMessage {
        turn_id: TurnId,
        parts: Vec<SessionUserPart>,
    },
    /// Records assistant tool calls before execution.
    AssistantToolCallBatch {
        turn_id: TurnId,
        model_call_id: String,
        tool_batch_id: ToolBatchId,
        blocks: Vec<SessionAssistantBlock>,
    },
    /// Records an ordered tool result.
    ToolResult {
        turn_id: TurnId,
        tool_batch_id: ToolBatchId,
        result: SessionToolResult,
    },
    /// Records the final assistant message of a successful turn.
    AssistantMessage {
        turn_id: TurnId,
        blocks: Vec<SessionAssistantBlock>,
    },
    /// Records a normalized failure for an active turn.
    TurnFailure {
        turn_id: TurnId,
        failure: TurnFailure,
    },
    /// Records the terminal turn outcome.
    TurnTerminated {
        turn_id: TurnId,
        outcome: TurnOutcome,
    },
    /// Records the terminal operation outcome.
    OperationSettled {
        operation_id: OperationId,
        outcome: OperationOutcome,
        /// Token usage observed during this turn, when known. Only the
        /// newest settled turn's usage is projected by
        /// [`crate::SessionContextView`]; earlier turns keep the durable
        /// fact but do not surface it.
        usage: Option<SessionTokenUsage>,
        /// The generation choice used for this turn, when known. Only the
        /// newest settled turn's choice is projected by
        /// [`crate::SessionContextView`]; earlier turns keep the durable
        /// fact but do not surface it. The wire name is the persistent
        /// identity; the runtime `display_name` is not persisted.
        generation: Option<SessionGenerationChoice>,
    },
    /// Replaces the model-visible prefix through a settled operation with a
    /// durable summary. Original entries remain unchanged and replayable.
    Compaction {
        summary: String,
        covers_up_to: EntryId,
    },
    /// Records a human-readable session title. The newest `TitleSet` wins;
    /// sessions without one derive a display title from the first user text.
    TitleSet {
        /// The validated title text.
        title: String,
    },
}

/// Parent reference used before transaction entry IDs are allocated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewEntryParent {
    /// The current committed leaf, including an empty session's absent leaf.
    CurrentLeaf,
    /// An earlier entry in the same transaction by zero-based index.
    TransactionEntry(usize),
}

/// One not-yet-committed session entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewSessionEntry {
    pub(crate) parent: NewEntryParent,
    pub(crate) kind: SessionEntryKind,
}

impl NewSessionEntry {
    /// Creates the first entry in a transaction, attached to the current leaf.
    pub fn at_current_leaf(kind: SessionEntryKind) -> Self {
        Self {
            parent: NewEntryParent::CurrentLeaf,
            kind,
        }
    }

    /// Creates an entry attached to an earlier transaction entry.
    pub fn after(transaction_index: usize, kind: SessionEntryKind) -> Self {
        Self {
            parent: NewEntryParent::TransactionEntry(transaction_index),
            kind,
        }
    }

    /// Returns the requested parent relation.
    pub fn parent(&self) -> &NewEntryParent {
        &self.parent
    }

    /// Returns the durable entry payload.
    pub fn kind(&self) -> &SessionEntryKind {
        &self.kind
    }
}

/// An atomic append request guarded by an expected revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTransaction {
    pub(crate) session_id: SessionId,
    pub(crate) expected_revision: SessionRevision,
    pub(crate) entries: Vec<NewSessionEntry>,
    pub(crate) current_leaf_index: usize,
}

impl SessionTransaction {
    /// Creates a transaction with an explicit logical current-leaf result.
    pub fn new(
        session_id: SessionId,
        expected_revision: SessionRevision,
        entries: Vec<NewSessionEntry>,
        current_leaf_index: usize,
    ) -> Self {
        Self {
            session_id,
            expected_revision,
            entries,
            current_leaf_index,
        }
    }

    /// Creates a valid linear transaction whose last entry becomes the leaf.
    pub fn linear(
        session_id: SessionId,
        expected_revision: SessionRevision,
        kinds: Vec<SessionEntryKind>,
    ) -> Self {
        let entries = kinds
            .into_iter()
            .enumerate()
            .map(|(index, kind)| {
                if index == 0 {
                    NewSessionEntry::at_current_leaf(kind)
                } else {
                    NewSessionEntry::after(index - 1, kind)
                }
            })
            .collect::<Vec<_>>();
        let current_leaf_index = entries.len().saturating_sub(1);
        Self::new(session_id, expected_revision, entries, current_leaf_index)
    }

    /// Returns the target session.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Returns the optimistic-concurrency revision.
    pub fn expected_revision(&self) -> SessionRevision {
        self.expected_revision
    }

    /// Returns the ordered new entries.
    pub fn entries(&self) -> &[NewSessionEntry] {
        &self.entries
    }

    /// Returns the transaction index that must become the new current leaf.
    pub fn current_leaf_index(&self) -> usize {
        self.current_leaf_index
    }
}

/// One committed, immutable session entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEntry {
    pub(crate) id: EntryId,
    pub(crate) parent: Option<EntryId>,
    pub(crate) kind: SessionEntryKind,
}

impl SessionEntry {
    /// Rebuilds a previously committed entry from its persisted fields.
    ///
    /// This path only serves replaying existing durable facts: a rebuilt
    /// entry still has to pass `SessionProjection::replay` validation before
    /// it can enter a projection, so it is not a channel for forging history.
    pub fn from_persisted(id: EntryId, parent: Option<EntryId>, kind: SessionEntryKind) -> Self {
        Self { id, parent, kind }
    }

    /// Returns the store-assigned entry identifier.
    pub fn id(&self) -> &EntryId {
        &self.id
    }

    /// Returns the previous entry in the linear chain, if any.
    pub fn parent(&self) -> Option<&EntryId> {
        self.parent.as_ref()
    }

    /// Returns the durable payload.
    pub fn kind(&self) -> &SessionEntryKind {
        &self.kind
    }
}

/// Result of a successful atomic transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCommit {
    pub(crate) revision: SessionRevision,
    pub(crate) entries: Vec<SessionEntry>,
    pub(crate) current_leaf: EntryId,
}

impl SessionCommit {
    /// Returns the revision after the commit.
    pub fn revision(&self) -> SessionRevision {
        self.revision
    }

    /// Returns the entries atomically appended by this commit.
    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    /// Returns the new current leaf.
    pub fn current_leaf(&self) -> &EntryId {
        &self.current_leaf
    }
}
