//! Backend-agnostic validation core shared by every session store.
//!
//! [`SessionProjection`] owns transaction validation, lifecycle state
//! derivation, deterministic [`EntryId`] allocation, and the model context
//! projection. Stores commit through [`SessionProjection::apply`] and rebuild
//! durable history through [`SessionProjection::replay`]; both paths enforce
//! exactly the same rules, so no backend can drift or forge history.
//!
//! The core is pure: no I/O, no async, and results depend only on inputs.

use std::collections::HashMap;

use crate::entry::{
    CancelReason, EntryId, NewEntryParent, OperationId, OperationOutcome, SessionCommit,
    SessionEntry, SessionEntryKind, SessionId, SessionRevision, SessionTransaction,
    SessionUserPart, ToolBatchId, ToolCallId, TurnFailure, TurnId, TurnOutcome,
};
use crate::store::{SessionError, SessionValidationError};
use crate::tool_entry::ToolResultOutcome;
use crate::view::{ContextMessage, OpenTurnInfo, SessionContextView, UnfilledBatch};

/// Validated, replayable state of one session's linear active path.
#[derive(Clone, Debug, Default)]
pub struct SessionProjection {
    revision: SessionRevision,
    current_leaf: Option<EntryId>,
    entry_count: usize,
    operations: HashMap<OperationId, OperationRecord>,
    turns: HashMap<TurnId, TurnRecord>,
    /// Turns without a durable terminal outcome, in start order.
    open_turn_ids: Vec<TurnId>,
    messages: Vec<ContextMessage>,
}

#[derive(Clone, Debug)]
struct OperationRecord {
    turn_id: Option<TurnId>,
    outcome: Option<OperationOutcome>,
}

#[derive(Clone, Debug)]
struct TurnRecord {
    operation_id: OperationId,
    has_user_message: bool,
    has_assistant_message: bool,
    failure: Option<TurnFailure>,
    outcome: Option<TurnOutcome>,
    tool_batches: Vec<ToolBatchRecord>,
}

impl TurnRecord {
    /// The turn's tool facts are settled when its newest batch has all results.
    fn tool_rounds_complete(&self) -> bool {
        self.tool_batches
            .last()
            .is_none_or(ToolBatchRecord::is_complete)
    }
}

#[derive(Clone, Debug)]
struct ToolBatchRecord {
    batch_id: ToolBatchId,
    call_ids: Vec<ToolCallId>,
    results: Vec<ToolCallId>,
}

impl ToolBatchRecord {
    fn is_complete(&self) -> bool {
        self.results.len() == self.call_ids.len()
    }
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
        let mut next = self.clone();
        next.validate_tool_result_purity(transaction.entries.iter().map(|entry| entry.kind()))?;

        let mut committed: Vec<SessionEntry> = Vec::with_capacity(transaction.entries.len());
        for (index, new_entry) in transaction.entries.iter().enumerate() {
            let parent = match (new_entry.parent(), index) {
                (NewEntryParent::CurrentLeaf, 0) => next.current_leaf.clone(),
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
            next.accept_entry_kind(new_entry.kind())?;
            let id = next.allocate_entry_id(&transaction.session_id);
            next.entry_count += 1;
            next.project_message(new_entry.kind());
            committed.push(SessionEntry {
                id,
                parent,
                kind: new_entry.kind().clone(),
            });
        }
        next.current_leaf = Some(committed[transaction.current_leaf_index].id.clone());
        next.revision = SessionRevision::new(next.revision.get() + 1);
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
    /// On error the projection is no longer consistent and must be discarded.
    pub fn replay(&mut self, entries: &[SessionEntry]) -> Result<(), SessionError> {
        if entries.is_empty() {
            return validation(SessionValidationError::EmptyTransaction);
        }
        self.validate_tool_result_purity(entries.iter().map(SessionEntry::kind))?;
        for (index, entry) in entries.iter().enumerate() {
            let expected_parent = if index == 0 {
                self.current_leaf.as_ref()
            } else {
                Some(&entries[index - 1].id)
            };
            if entry.parent() != expected_parent {
                return validation(SessionValidationError::InvalidParent { entry_index: index });
            }
            self.accept_entry_kind(entry.kind())?;
            self.entry_count += 1;
            self.project_message(entry.kind());
        }
        self.current_leaf = Some(entries.last().expect("entries checked nonempty").id.clone());
        self.revision = SessionRevision::new(self.revision.get() + 1);
        Ok(())
    }

    /// Projects the model-visible context from this projection's state.
    pub fn context_view(&self, session_id: &SessionId) -> SessionContextView {
        let open_turns = self
            .open_turn_ids
            .iter()
            .map(|turn_id| {
                let turn = self
                    .turns
                    .get(turn_id)
                    .expect("every open turn id has a turn record");
                let unfilled_batch = turn.tool_batches.last().and_then(|batch| {
                    if batch.is_complete() {
                        return None;
                    }
                    Some(UnfilledBatch {
                        tool_batch_id: batch.batch_id.clone(),
                        unfilled_call_ids: batch.call_ids[batch.results.len()..].to_vec(),
                    })
                });
                OpenTurnInfo {
                    operation_id: turn.operation_id.clone(),
                    turn_id: turn_id.clone(),
                    unfilled_batch,
                }
            })
            .collect();
        SessionContextView {
            session_id: session_id.clone(),
            revision: self.revision,
            current_leaf: self.current_leaf.clone(),
            messages: self.messages.clone(),
            open_turns,
        }
    }

    /// Allocates the next deterministic entry ID. The format is a durable
    /// fact once persisted; readers treat IDs as opaque strings.
    fn allocate_entry_id(&self, session_id: &SessionId) -> EntryId {
        EntryId::new(format!(
            "{}:entry:{}",
            session_id.as_str(),
            self.entry_count + 1
        ))
    }

    fn project_message(&mut self, kind: &SessionEntryKind) {
        match kind {
            SessionEntryKind::UserMessage { parts, .. } => {
                self.messages.push(ContextMessage::User {
                    parts: parts.clone(),
                });
            }
            SessionEntryKind::AssistantMessage { content, .. } => {
                self.messages.push(ContextMessage::Assistant {
                    content: content.clone(),
                });
            }
            SessionEntryKind::AssistantToolCallBatch {
                tool_batch_id,
                calls,
                ..
            } => {
                self.messages.push(ContextMessage::AssistantToolCalls {
                    tool_batch_id: tool_batch_id.clone(),
                    calls: calls.clone(),
                });
            }
            SessionEntryKind::ToolResult { result, .. } => {
                self.messages.push(ContextMessage::ToolResult {
                    tool_call_id: result.call_id().clone(),
                    outcome: result.outcome().clone(),
                });
            }
            _ => {}
        }
    }

    fn accept_entry_kind(&mut self, kind: &SessionEntryKind) -> Result<(), SessionError> {
        match kind {
            SessionEntryKind::OperationStarted { operation_id } => {
                if self.operations.contains_key(operation_id) {
                    return validation(SessionValidationError::DuplicateOperation {
                        operation_id: operation_id.clone(),
                    });
                }
                self.operations.insert(
                    operation_id.clone(),
                    OperationRecord {
                        turn_id: None,
                        outcome: None,
                    },
                );
            }
            SessionEntryKind::TurnStarted {
                operation_id,
                turn_id,
            } => {
                let operation = self.active_operation(operation_id)?;
                if operation.turn_id.is_some() {
                    return validation(SessionValidationError::InvalidOperationReference {
                        operation_id: operation_id.clone(),
                    });
                }
                if self.turns.contains_key(turn_id) {
                    return validation(SessionValidationError::DuplicateTurn {
                        turn_id: turn_id.clone(),
                    });
                }
                self.operations
                    .get_mut(operation_id)
                    .expect("active operation was just validated")
                    .turn_id = Some(turn_id.clone());
                self.turns.insert(
                    turn_id.clone(),
                    TurnRecord {
                        operation_id: operation_id.clone(),
                        has_user_message: false,
                        has_assistant_message: false,
                        failure: None,
                        outcome: None,
                        tool_batches: Vec::new(),
                    },
                );
                self.open_turn_ids.push(turn_id.clone());
            }
            SessionEntryKind::UserMessage { turn_id, parts } => {
                // Structural rules only: parts non-empty, text parts
                // non-empty, media type present. Media content validity
                // stays an adapter/SDK concern.
                let well_formed = !parts.is_empty()
                    && parts.iter().all(|part| match part {
                        SessionUserPart::Text(text) => !text.is_empty(),
                        SessionUserPart::Image { media_type, .. } => !media_type.is_empty(),
                    });
                if !well_formed {
                    return validation(SessionValidationError::InvalidUserMessage {
                        turn_id: turn_id.clone(),
                    });
                }
                let turn = self.active_turn_mut(turn_id)?;
                if turn.has_user_message {
                    return validation(SessionValidationError::DuplicateUserMessage {
                        turn_id: turn_id.clone(),
                    });
                }
                turn.has_user_message = true;
            }
            SessionEntryKind::AssistantToolCallBatch {
                turn_id,
                tool_batch_id,
                calls,
                ..
            } => {
                let turn = self.active_turn_mut(turn_id)?;
                let unique = calls
                    .iter()
                    .map(|call| call.id())
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    == calls.len();
                let batch_id_unused = turn
                    .tool_batches
                    .iter()
                    .all(|batch| batch.batch_id != *tool_batch_id);
                if !turn.has_user_message
                    || turn.has_assistant_message
                    || turn.failure.is_some()
                    || !turn.tool_rounds_complete()
                    || !batch_id_unused
                    || calls.is_empty()
                    || !unique
                {
                    return validation(SessionValidationError::InvalidToolBatch {
                        turn_id: turn_id.clone(),
                    });
                }
                turn.tool_batches.push(ToolBatchRecord {
                    batch_id: tool_batch_id.clone(),
                    call_ids: calls.iter().map(|call| call.id().clone()).collect(),
                    results: Vec::new(),
                });
            }
            SessionEntryKind::ToolResult {
                turn_id,
                tool_batch_id,
                result,
            } => {
                let turn = self.active_turn_mut(turn_id)?;
                // Results may only complete the newest batch, in source order.
                let Some(batch) = turn.tool_batches.last_mut() else {
                    return validation(SessionValidationError::InvalidToolResult {
                        turn_id: turn_id.clone(),
                    });
                };
                let index = batch.results.len();
                if batch.batch_id != *tool_batch_id
                    || batch.call_ids.get(index) != Some(result.call_id())
                    || batch.results.contains(result.call_id())
                {
                    return validation(SessionValidationError::InvalidToolResult {
                        turn_id: turn_id.clone(),
                    });
                }
                batch.results.push(result.call_id().clone());
            }
            SessionEntryKind::AssistantMessage { turn_id, .. } => {
                let turn = self.active_turn_mut(turn_id)?;
                let tools_complete = turn.tool_rounds_complete();
                if !turn.has_user_message || turn.failure.is_some() || !tools_complete {
                    return validation(SessionValidationError::InvalidTurnReference {
                        turn_id: turn_id.clone(),
                    });
                }
                if turn.has_assistant_message {
                    return validation(SessionValidationError::DuplicateAssistantMessage {
                        turn_id: turn_id.clone(),
                    });
                }
                turn.has_assistant_message = true;
            }
            SessionEntryKind::TurnFailure { turn_id, failure } => {
                let turn = self.active_turn_mut(turn_id)?;
                if !turn.has_user_message || turn.has_assistant_message || turn.failure.is_some() {
                    return validation(SessionValidationError::InvalidTurnReference {
                        turn_id: turn_id.clone(),
                    });
                }
                turn.failure = Some(failure.clone());
            }
            SessionEntryKind::TurnTerminated { turn_id, outcome } => {
                let turn = self.active_turn_mut(turn_id)?;
                let valid = match outcome {
                    TurnOutcome::Succeeded => turn.has_assistant_message && turn.failure.is_none(),
                    TurnOutcome::Failed => turn.failure.is_some() && !turn.has_assistant_message,
                    // Entries commit in order, so completion marks appended by
                    // this same transaction are already visible here.
                    TurnOutcome::Cancelled { .. } => {
                        !turn.has_assistant_message
                            && turn.failure.is_none()
                            && turn.tool_rounds_complete()
                    }
                };
                if !valid {
                    return validation(SessionValidationError::InvalidTurnOutcome {
                        turn_id: turn_id.clone(),
                    });
                }
                turn.outcome = Some(*outcome);
                self.open_turn_ids.retain(|open| open != turn_id);
            }
            SessionEntryKind::OperationSettled {
                operation_id,
                outcome,
            } => {
                let operation = self.active_operation(operation_id)?;
                let Some(turn_id) = operation.turn_id.clone() else {
                    return validation(SessionValidationError::InvalidOperationOutcome {
                        operation_id: operation_id.clone(),
                    });
                };
                // Cancellation reasons must agree between the operation and
                // its turn within one transaction.
                let expected_turn_outcome = match outcome {
                    OperationOutcome::Succeeded => TurnOutcome::Succeeded,
                    OperationOutcome::Failed => TurnOutcome::Failed,
                    OperationOutcome::Cancelled { reason } => {
                        TurnOutcome::Cancelled { reason: *reason }
                    }
                };
                let valid = self.turns.get(&turn_id).is_some_and(|turn| {
                    turn.operation_id == *operation_id
                        && turn.outcome == Some(expected_turn_outcome)
                });
                if !valid {
                    return validation(SessionValidationError::InvalidOperationOutcome {
                        operation_id: operation_id.clone(),
                    });
                }
                self.operations
                    .get_mut(operation_id)
                    .expect("active operation was just validated")
                    .outcome = Some(*outcome);
            }
        }
        Ok(())
    }

    fn active_operation(
        &self,
        operation_id: &OperationId,
    ) -> Result<&OperationRecord, SessionError> {
        match self.operations.get(operation_id) {
            Some(operation) if operation.outcome.is_none() => Ok(operation),
            _ => validation(SessionValidationError::InvalidOperationReference {
                operation_id: operation_id.clone(),
            }),
        }
    }

    fn active_turn_mut(&mut self, turn_id: &TurnId) -> Result<&mut TurnRecord, SessionError> {
        match self.turns.get_mut(turn_id) {
            Some(turn) if turn.outcome.is_none() => Ok(turn),
            _ => validation(SessionValidationError::InvalidTurnReference {
                turn_id: turn_id.clone(),
            }),
        }
    }

    /// Enforces that one transaction either atomically completes the newest
    /// durable tool batch of one turn, or contains no tool results at all,
    /// and that an assistant message never lands before the turn's newest
    /// batch is fully resolved.
    fn validate_tool_result_purity<'a>(
        &self,
        kinds: impl Iterator<Item = &'a SessionEntryKind> + Clone,
    ) -> Result<(), SessionError> {
        let result_entries = kinds
            .clone()
            .filter_map(|kind| match kind {
                SessionEntryKind::ToolResult {
                    turn_id,
                    tool_batch_id,
                    result,
                } => Some((turn_id, tool_batch_id, result)),
                _ => None,
            })
            .collect::<Vec<_>>();
        if result_entries.is_empty() {
            let incomplete_assistant_turn = kinds.clone().find_map(|kind| match kind {
                SessionEntryKind::AssistantMessage { turn_id, .. } => {
                    let incomplete = self
                        .turns
                        .get(turn_id)
                        .is_some_and(|turn| !turn.tool_rounds_complete());
                    incomplete.then(|| turn_id.clone())
                }
                _ => None,
            });
            if let Some(turn_id) = incomplete_assistant_turn {
                return validation(SessionValidationError::InvalidTurnOutcome { turn_id });
            }
            return Ok(());
        }
        // A tool-result save point carries nothing but the batch's results.
        if kinds.clone().any(|kind| {
            matches!(
                kind,
                SessionEntryKind::AssistantMessage { .. }
                    | SessionEntryKind::AssistantToolCallBatch { .. }
            )
        }) {
            return validation(SessionValidationError::InvalidTurnOutcome {
                turn_id: result_entries[0].0.clone(),
            });
        }
        let turn_id = result_entries[0].0;
        let Some(turn) = self.turns.get(turn_id) else {
            return validation(SessionValidationError::InvalidToolResult {
                turn_id: turn_id.clone(),
            });
        };
        let Some(batch) = turn.tool_batches.last() else {
            return validation(SessionValidationError::InvalidToolResult {
                turn_id: turn_id.clone(),
            });
        };
        let expected = &batch.call_ids[batch.results.len()..];
        let exact = result_entries.len() == expected.len()
            && result_entries.iter().enumerate().all(
                |(index, (entry_turn, entry_batch, result))| {
                    *entry_turn == turn_id
                        && *entry_batch == &batch.batch_id
                        && result.call_id() == &expected[index]
                },
            );
        if !exact {
            return validation(SessionValidationError::InvalidToolResult {
                turn_id: turn_id.clone(),
            });
        }
        // Completion-mark matrix: which outcomes a transaction may carry
        // depends on how (and whether) it cancels the turn.
        //
        //   no cancellation        only real results (plain C_k)
        //   User / Timeout         real source-order prefix + Cancelled suffix
        //   Abandoned (seal)       every completion mark is Interrupted
        let cancel_reason = kinds.clone().find_map(|kind| match kind {
            SessionEntryKind::TurnTerminated {
                turn_id: terminated,
                outcome: TurnOutcome::Cancelled { reason },
            } if terminated == turn_id => Some(*reason),
            _ => None,
        });
        let first_cancelled = result_entries
            .iter()
            .position(|(_, _, result)| matches!(result.outcome(), ToolResultOutcome::Cancelled));
        let has_interrupted = result_entries
            .iter()
            .any(|(_, _, result)| matches!(result.outcome(), ToolResultOutcome::Interrupted));
        let valid_marks = match cancel_reason {
            // C_k atomicity means an abandoned batch has zero durable
            // results, so a seal completes it with Interrupted only: there
            // is no real prefix to preserve.
            Some(CancelReason::Abandoned) => result_entries
                .iter()
                .all(|(_, _, result)| matches!(result.outcome(), ToolResultOutcome::Interrupted)),
            Some(_) => {
                !has_interrupted
                    && first_cancelled.is_none_or(|first| {
                        result_entries[first..].iter().all(|(_, _, result)| {
                            matches!(result.outcome(), ToolResultOutcome::Cancelled)
                        })
                    })
            }
            None => !has_interrupted && first_cancelled.is_none(),
        };
        if !valid_marks {
            return validation(SessionValidationError::InvalidToolResult {
                turn_id: turn_id.clone(),
            });
        }
        Ok(())
    }
}

fn validation<T>(error: SessionValidationError) -> Result<T, SessionError> {
    Err(SessionError::Validation(error))
}
