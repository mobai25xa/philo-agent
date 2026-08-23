//! Transaction materialization, chain validation, and shared folding.

use crate::entry::{
    NewEntryParent, SessionEntry, SessionEntryKind, SessionRevision, SessionTransaction,
};
use crate::error::{SessionError, SessionValidationError, validation};

use super::{AppliedTransaction, SessionProjection};

impl SessionProjection {
    /// Validates one transaction against this projection, allocates
    /// deterministic entry IDs, and returns the committed facts together with
    /// the advanced projection. `self` is not modified.
    ///
    /// Revision-conflict detection against the store's durable revision stays
    /// a store responsibility; the comparison value must come from
    /// [`SessionProjection::revision`].
    pub fn apply(
        &self,
        transaction: &SessionTransaction,
    ) -> Result<AppliedTransaction, SessionError> {
        if transaction.entries.is_empty() {
            return validation(SessionValidationError::EmptyTransaction);
        }
        if transaction.current_leaf_index != transaction.entries.len() - 1 {
            return validation(SessionValidationError::InvalidCurrentLeaf);
        }

        let mut committed: Vec<SessionEntry> = Vec::with_capacity(transaction.entries.len());
        for (index, new_entry) in transaction.entries.iter().enumerate() {
            let parent = match (new_entry.parent(), index) {
                (NewEntryParent::CurrentLeaf, 0) => self.current_leaf.clone(),
                (NewEntryParent::TransactionEntry(parent_index), current_index)
                    if current_index > 0 && *parent_index == current_index - 1 =>
                {
                    Some(committed[*parent_index].id.clone())
                }
                _ => {
                    return validation(SessionValidationError::InvalidParent {
                        entry_index: index,
                    });
                }
            };
            committed.push(SessionEntry {
                id: self.allocate_entry_id(&transaction.session_id, index),
                parent,
                kind: new_entry.kind().clone(),
            });
        }

        let mut next = self.clone();
        next.advance_transaction(&committed)?;
        Ok(AppliedTransaction {
            entries: committed,
            projection: next,
        })
    }

    /// Replays the ordered entries of one previously committed transaction,
    /// advancing this projection by exactly one revision. Validation rules
    /// match [`SessionProjection::apply`]; broken chains, dangling references,
    /// or out-of-order facts are rejected.
    ///
    /// The projection is not modified when validation fails.
    pub fn replay(&mut self, entries: &[SessionEntry]) -> Result<(), SessionError> {
        let mut next = self.clone();
        next.advance_transaction(entries)?;
        *self = next;
        Ok(())
    }

    fn advance_transaction(&mut self, entries: &[SessionEntry]) -> Result<(), SessionError> {
        if entries.is_empty() {
            return validation(SessionValidationError::EmptyTransaction);
        }
        validate_compaction_transaction(entries)?;
        self.lifecycle
            .validate_transaction(entries.iter().map(SessionEntry::kind))?;
        for (index, entry) in entries.iter().enumerate() {
            let expected_parent = if index == 0 {
                self.current_leaf.as_ref()
            } else {
                Some(&entries[index - 1].id)
            };
            if entry.parent() != expected_parent {
                return validation(SessionValidationError::InvalidParent { entry_index: index });
            }
            match entry.kind() {
                SessionEntryKind::TitleSet { title } if !super::title_is_valid(title) => {
                    return validation(SessionValidationError::InvalidTitle);
                }
                SessionEntryKind::TitleSet { title } => {
                    self.title_override = Some(title.trim().to_owned());
                }
                _ => {}
            }
            self.lifecycle.accept(entry.kind())?;
            self.context.accept(entry, self.entry_count)?;
            self.entry_count += 1;
        }
        self.current_leaf = Some(entries.last().expect("entries checked nonempty").id.clone());
        self.revision = SessionRevision::new(self.revision.get() + 1);
        Ok(())
    }
}

fn validate_compaction_transaction(entries: &[SessionEntry]) -> Result<(), SessionError> {
    let contains_compaction = entries
        .iter()
        .any(|entry| matches!(entry.kind(), SessionEntryKind::Compaction { .. }));
    if contains_compaction && entries.len() != 1 {
        return validation(SessionValidationError::InvalidCompactionTransaction);
    }
    Ok(())
}
