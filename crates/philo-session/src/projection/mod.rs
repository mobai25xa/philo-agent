//! Backend-agnostic validation core shared by every session store.
//!
//! [`SessionProjection`] owns transaction validation, lifecycle state
//! derivation, deterministic [`EntryId`] allocation, and the model context
//! projection. Stores commit through [`SessionProjection::apply`] and rebuild
//! durable history through [`SessionProjection::replay`]; both paths enforce
//! exactly the same rules, so no backend can drift or forge history.
//!
//! The core is pure: no I/O, no async, and results depend only on inputs.

mod context;
mod lifecycle;
mod transaction;

use context::ContextProjection;
use lifecycle::LifecycleProjection;

use crate::entry::{EntryId, SessionCommit, SessionEntry, SessionId, SessionRevision};
use crate::view::SessionContextView;

/// Validated, replayable state of one session's linear active path.
#[derive(Clone, Debug, Default)]
pub struct SessionProjection {
    revision: SessionRevision,
    current_leaf: Option<EntryId>,
    entry_count: usize,
    lifecycle: LifecycleProjection,
    context: ContextProjection,
}

/// Output of a validated [`SessionProjection::apply`]: the committed facts and
/// the advanced projection.
#[derive(Clone, Debug)]
pub struct AppliedTransaction {
    entries: Vec<SessionEntry>,
    projection: SessionProjection,
}

impl AppliedTransaction {
    /// Returns the entries committed by this transaction, in order.
    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    /// Returns the projection advanced past this transaction.
    pub fn projection(&self) -> &SessionProjection {
        &self.projection
    }

    /// Builds the store-facing commit record for this transaction.
    pub fn commit(&self) -> SessionCommit {
        SessionCommit {
            revision: self.projection.revision,
            entries: self.entries.clone(),
            current_leaf: self
                .projection
                .current_leaf
                .clone()
                .expect("a validated nonempty transaction sets a leaf"),
        }
    }

    /// Consumes this result, yielding the advanced projection.
    pub fn into_projection(self) -> SessionProjection {
        self.projection
    }
}

impl SessionProjection {
    /// Creates the projection of a session with no committed transactions.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Returns the revision after the last applied or replayed transaction.
    pub fn revision(&self) -> SessionRevision {
        self.revision
    }

    /// Returns the current leaf, absent for an empty session.
    pub fn current_leaf(&self) -> Option<&EntryId> {
        self.current_leaf.as_ref()
    }

    /// Projects the model-visible context from this projection's state.
    pub fn context_view(&self, session_id: &SessionId) -> SessionContextView {
        SessionContextView {
            session_id: session_id.clone(),
            revision: self.revision,
            current_leaf: self.current_leaf.clone(),
            messages: self.context.messages(),
            open_turns: self.lifecycle.open_turns(),
            settled_turn_boundaries: self.context.settled_boundary_ids(),
            latest_compaction_boundary: self.context.latest_compaction_boundary(),
        }
    }

    /// Allocates the next deterministic entry ID. The format is a durable
    /// fact once persisted; readers treat IDs as opaque strings.
    fn allocate_entry_id(&self, session_id: &SessionId, offset: usize) -> EntryId {
        EntryId::new(format!(
            "{}:entry:{}",
            session_id.as_str(),
            self.entry_count + offset + 1
        ))
    }
}
