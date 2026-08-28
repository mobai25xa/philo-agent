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

use crate::entry::{
    EntryId, SessionCommit, SessionEntry, SessionId, SessionRevision, SessionUserPart,
};
use crate::view::SessionContextView;

/// Longest title a `TitleSet` entry may carry. Derived titles are shorter.
pub const TITLE_OVERRIDE_MAX_CHARS: usize = 200;

/// Longest derived title taken from the first user text part.
const DERIVED_TITLE_MAX_CHARS: usize = 48;

/// Normalizes one user text part into a candidate display title: whitespace
/// runs collapse to single spaces and long texts truncate at a char boundary
/// with an ellipsis. Empty results yield `None`.
pub fn derive_title(text: &str) -> Option<String> {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    if normalized.chars().count() <= DERIVED_TITLE_MAX_CHARS {
        return Some(normalized);
    }
    let truncated = normalized
        .chars()
        .take(DERIVED_TITLE_MAX_CHARS)
        .collect::<String>();
    Some(format!("{truncated}…"))
}

/// Whether a `TitleSet` payload is structurally valid.
pub(crate) fn title_is_valid(title: &str) -> bool {
    let trimmed = title.trim();
    !trimmed.is_empty()
        && trimmed.chars().count() <= TITLE_OVERRIDE_MAX_CHARS
        && !trimmed.chars().any(char::is_control)
}

/// Validated, replayable state of one session's linear active path.
#[derive(Clone, Debug, Default)]
pub struct SessionProjection {
    revision: SessionRevision,
    current_leaf: Option<EntryId>,
    entry_count: usize,
    lifecycle: LifecycleProjection,
    context: ContextProjection,
    /// The newest valid `TitleSet` payload. Absent until an explicit rename.
    title_override: Option<String>,
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
            title: self.title(),
            messages: self.context.messages(),
            open_turns: self.lifecycle.open_turns(),
            settled_turns: self.lifecycle.settled_turns(),
            settled_turn_boundaries: self.context.settled_boundary_ids(),
            latest_compaction_boundary: self.context.latest_compaction_boundary(),
            latest_usage: self.context.latest_usage(),
            latest_generation: self.context.latest_generation().cloned(),
        }
    }

    /// Resolves the display title: the newest `TitleSet` override, else a
    /// title derived from the first user text part, else `None`.
    pub fn title(&self) -> Option<String> {
        if let Some(title) = &self.title_override {
            return Some(title.clone());
        }
        let first = self.context.source_messages.first()?;
        match first {
            crate::view::ContextMessage::User { parts } => parts.iter().find_map(|part| {
                match part {
                    SessionUserPart::Text(text) => derive_title(text),
                    SessionUserPart::Image { .. } => None,
                }
            }),
            _ => None,
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
