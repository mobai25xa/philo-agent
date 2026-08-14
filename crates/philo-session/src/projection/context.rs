//! Model-visible message and compaction-prefix projection.

use std::collections::HashMap;

use crate::entry::{EntryId, SessionEntry, SessionEntryKind};
use crate::error::{SessionError, SessionValidationError, validation};
use crate::view::ContextMessage;

#[derive(Clone, Debug, Default)]
pub(super) struct ContextProjection {
    source_messages: Vec<ContextMessage>,
    settled_boundaries: HashMap<EntryId, SettledBoundary>,
    latest_compaction: Option<CompactionState>,
}

#[derive(Clone, Debug)]
struct SettledBoundary {
    entry_ordinal: usize,
    message_count: usize,
}

#[derive(Clone, Debug)]
struct CompactionState {
    summary: String,
    covers_up_to: EntryId,
    boundary: SettledBoundary,
}

impl ContextProjection {
    pub(super) fn settled_boundary_ids(&self) -> Vec<EntryId> {
        let mut boundaries = self.settled_boundaries.iter().collect::<Vec<_>>();
        boundaries.sort_by_key(|(_, boundary)| boundary.entry_ordinal);
        boundaries
            .into_iter()
            .map(|(entry_id, _)| entry_id.clone())
            .collect()
    }

    pub(super) fn latest_compaction_boundary(&self) -> Option<EntryId> {
        self.latest_compaction
            .as_ref()
            .map(|compaction| compaction.covers_up_to.clone())
    }

    pub(super) fn accept(
        &mut self,
        entry: &SessionEntry,
        entry_ordinal: usize,
    ) -> Result<(), SessionError> {
        match entry.kind() {
            SessionEntryKind::UserMessage { parts, .. } => {
                self.source_messages.push(ContextMessage::User {
                    parts: parts.clone(),
                });
            }
            SessionEntryKind::AssistantMessage { content, .. } => {
                self.source_messages.push(ContextMessage::Assistant {
                    content: content.clone(),
                });
            }
            SessionEntryKind::AssistantToolCallBatch {
                tool_batch_id,
                calls,
                ..
            } => {
                self.source_messages
                    .push(ContextMessage::AssistantToolCalls {
                        tool_batch_id: tool_batch_id.clone(),
                        calls: calls.clone(),
                    });
            }
            SessionEntryKind::ToolResult { result, .. } => {
                self.source_messages.push(ContextMessage::ToolResult {
                    tool_call_id: result.call_id().clone(),
                    outcome: result.outcome().clone(),
                });
            }
            SessionEntryKind::OperationSettled { .. } => {
                self.settled_boundaries.insert(
                    entry.id().clone(),
                    SettledBoundary {
                        entry_ordinal,
                        message_count: self.source_messages.len(),
                    },
                );
            }
            SessionEntryKind::Compaction {
                summary,
                covers_up_to,
            } => {
                if summary.is_empty() {
                    return validation(SessionValidationError::InvalidCompactionSummary);
                }
                let Some(boundary) = self.settled_boundaries.get(covers_up_to).cloned() else {
                    return validation(SessionValidationError::InvalidCompactionBoundary {
                        covers_up_to: covers_up_to.clone(),
                    });
                };
                if let Some(previous) = &self.latest_compaction
                    && boundary.entry_ordinal <= previous.boundary.entry_ordinal
                {
                    return validation(SessionValidationError::NonMonotonicCompactionBoundary {
                        previous: previous.covers_up_to.clone(),
                        covers_up_to: covers_up_to.clone(),
                    });
                }
                self.latest_compaction = Some(CompactionState {
                    summary: summary.clone(),
                    covers_up_to: covers_up_to.clone(),
                    boundary,
                });
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn messages(&self) -> Vec<ContextMessage> {
        let Some(compaction) = &self.latest_compaction else {
            return self.source_messages.clone();
        };
        let tail = &self.source_messages[compaction.boundary.message_count..];
        let mut messages = Vec::with_capacity(tail.len() + 1);
        messages.push(ContextMessage::Summary {
            text: compaction.summary.clone(),
        });
        messages.extend_from_slice(tail);
        messages
    }
}
