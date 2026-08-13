//! On-disk schema v1 record types and their explicit mapping to the
//! `philo-session` public types.
//!
//! These records are owned by this crate: the disk schema never couples to
//! in-memory type refactorings. Field names and shapes are pinned by the
//! golden format tests. Entry and parent IDs are persisted as opaque strings.

use philo_session::{
    CancelReason, EntryId, OperationId, OperationOutcome, SessionEntry, SessionEntryKind,
    SessionToolCall, SessionToolResult, SessionUserPart, ToolBatchId, ToolCallId,
    ToolResultOutcome, TurnFailure, TurnFailureKind, TurnId, TurnOutcome,
};
use serde::{Deserialize, Serialize};

use crate::artifact::sha256_hex;

/// Envelope schema version written by this crate.
pub(crate) const SCHEMA_VERSION: u64 = 1;

/// One committed transaction: exactly one log line.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct TransactionRecord {
    pub v: u64,
    pub revision: u64,
    pub entries: Vec<EntryRecord>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct EntryRecord {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub kind: KindRecord,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum KindRecord {
    OperationStarted {
        operation_id: String,
    },
    TurnStarted {
        operation_id: String,
        turn_id: String,
    },
    /// New files always write `parts`; `content` is the pre-M8 legacy shape,
    /// read as a single text part. Both present or both absent is corrupt.
    UserMessage {
        turn_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parts: Option<Vec<UserPartRecord>>,
    },
    AssistantToolCallBatch {
        turn_id: String,
        model_call_id: String,
        tool_batch_id: String,
        calls: Vec<ToolCallRecord>,
    },
    ToolResult {
        turn_id: String,
        tool_batch_id: String,
        result: ToolResultRecord,
    },
    AssistantMessage {
        turn_id: String,
        content: String,
    },
    TurnFailure {
        turn_id: String,
        failure: FailureRecord,
    },
    /// Cancelled outcomes written since M11 carry a `reason`; legacy lines
    /// without one are read as user-requested cancellation (the only cancel
    /// source that existed before reasons were recorded).
    TurnTerminated {
        turn_id: String,
        outcome: OutcomeRecord,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<ReasonRecord>,
    },
    OperationSettled {
        operation_id: String,
        outcome: OutcomeRecord,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<ReasonRecord>,
    },
}

/// One part of a multi-part user message. Image bytes live in a
/// content-addressed artifact file; the record carries only the reference:
/// media type, artifact hash, and byte length (verified on both ends).
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum UserPartRecord {
    Text {
        text: String,
    },
    Image {
        media_type: String,
        artifact: String,
        len: u64,
    },
}

/// An artifact newly referenced by the transaction being encoded. The store
/// must make every pending artifact durable before appending the log line.
#[derive(Debug)]
pub(crate) struct PendingArtifact {
    pub hash: String,
    pub bytes: Vec<u8>,
}

/// Resolves an image reference (hash, recorded length) to verified bytes;
/// the error text describes the integrity failure.
pub(crate) type ArtifactLoader<'a> = dyn FnMut(&str, u64) -> Result<Vec<u8>, String> + 'a;

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ToolCallRecord {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ToolResultRecord {
    pub call_id: String,
    #[serde(flatten)]
    pub outcome: ToolOutcomeRecord,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum ToolOutcomeRecord {
    Success { content: String },
    Error { code: String, message: String },
    Cancelled,
    Interrupted,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct FailureRecord {
    pub kind: FailureKindRecord,
    pub message: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureKindRecord {
    ModelCall,
    InvalidModelOutput,
    Persistence,
    RuntimeDriver,
    ToolExecution,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutcomeRecord {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReasonRecord {
    User,
    Timeout,
    Abandoned,
}

fn encode_reason(reason: CancelReason) -> ReasonRecord {
    match reason {
        CancelReason::User => ReasonRecord::User,
        CancelReason::Timeout => ReasonRecord::Timeout,
        CancelReason::Abandoned => ReasonRecord::Abandoned,
    }
}

/// Legacy cancelled lines carry no reason: user cancellation was the only
/// cancel source before reasons were recorded.
fn decode_reason(reason: Option<ReasonRecord>) -> CancelReason {
    match reason {
        Some(ReasonRecord::User) | None => CancelReason::User,
        Some(ReasonRecord::Timeout) => CancelReason::Timeout,
        Some(ReasonRecord::Abandoned) => CancelReason::Abandoned,
    }
}

pub(crate) fn encode_entry(
    entry: &SessionEntry,
    pending: &mut Vec<PendingArtifact>,
) -> EntryRecord {
    EntryRecord {
        id: entry.id().as_str().to_owned(),
        parent: entry.parent().map(|parent| parent.as_str().to_owned()),
        kind: encode_kind(entry.kind(), pending),
    }
}

/// Decodes one persisted entry, resolving image references through
/// `load_artifact`.
pub(crate) fn decode_entry(
    record: EntryRecord,
    load_artifact: &mut ArtifactLoader<'_>,
) -> Result<SessionEntry, String> {
    Ok(SessionEntry::from_persisted(
        EntryId::new(record.id),
        record.parent.map(EntryId::new),
        decode_kind(record.kind, load_artifact)?,
    ))
}

fn encode_kind(kind: &SessionEntryKind, pending: &mut Vec<PendingArtifact>) -> KindRecord {
    match kind {
        SessionEntryKind::OperationStarted { operation_id } => KindRecord::OperationStarted {
            operation_id: operation_id.as_str().to_owned(),
        },
        SessionEntryKind::TurnStarted {
            operation_id,
            turn_id,
        } => KindRecord::TurnStarted {
            operation_id: operation_id.as_str().to_owned(),
            turn_id: turn_id.as_str().to_owned(),
        },
        SessionEntryKind::UserMessage { turn_id, parts } => KindRecord::UserMessage {
            turn_id: turn_id.as_str().to_owned(),
            content: None,
            parts: Some(
                parts
                    .iter()
                    .map(|part| match part {
                        SessionUserPart::Text(text) => UserPartRecord::Text { text: text.clone() },
                        SessionUserPart::Image { media_type, bytes } => {
                            let hash = sha256_hex(bytes);
                            if !pending.iter().any(|artifact| artifact.hash == hash) {
                                pending.push(PendingArtifact {
                                    hash: hash.clone(),
                                    bytes: bytes.clone(),
                                });
                            }
                            UserPartRecord::Image {
                                media_type: media_type.clone(),
                                artifact: hash,
                                len: bytes.len() as u64,
                            }
                        }
                    })
                    .collect(),
            ),
        },
        SessionEntryKind::AssistantToolCallBatch {
            turn_id,
            model_call_id,
            tool_batch_id,
            calls,
        } => KindRecord::AssistantToolCallBatch {
            turn_id: turn_id.as_str().to_owned(),
            model_call_id: model_call_id.clone(),
            tool_batch_id: tool_batch_id.as_str().to_owned(),
            calls: calls
                .iter()
                .map(|call| ToolCallRecord {
                    id: call.id().as_str().to_owned(),
                    name: call.name().to_owned(),
                    arguments: call.arguments().to_owned(),
                })
                .collect(),
        },
        SessionEntryKind::ToolResult {
            turn_id,
            tool_batch_id,
            result,
        } => KindRecord::ToolResult {
            turn_id: turn_id.as_str().to_owned(),
            tool_batch_id: tool_batch_id.as_str().to_owned(),
            result: ToolResultRecord {
                call_id: result.call_id().as_str().to_owned(),
                outcome: match result.outcome() {
                    ToolResultOutcome::Success { content } => ToolOutcomeRecord::Success {
                        content: content.clone(),
                    },
                    ToolResultOutcome::Error { code, message } => ToolOutcomeRecord::Error {
                        code: code.clone(),
                        message: message.clone(),
                    },
                    ToolResultOutcome::Cancelled => ToolOutcomeRecord::Cancelled,
                    ToolResultOutcome::Interrupted => ToolOutcomeRecord::Interrupted,
                },
            },
        },
        SessionEntryKind::AssistantMessage { turn_id, content } => KindRecord::AssistantMessage {
            turn_id: turn_id.as_str().to_owned(),
            content: content.clone(),
        },
        SessionEntryKind::TurnFailure { turn_id, failure } => KindRecord::TurnFailure {
            turn_id: turn_id.as_str().to_owned(),
            failure: FailureRecord {
                kind: match failure.kind() {
                    TurnFailureKind::ModelCall => FailureKindRecord::ModelCall,
                    TurnFailureKind::InvalidModelOutput => FailureKindRecord::InvalidModelOutput,
                    TurnFailureKind::Persistence => FailureKindRecord::Persistence,
                    TurnFailureKind::RuntimeDriver => FailureKindRecord::RuntimeDriver,
                    TurnFailureKind::ToolExecution => FailureKindRecord::ToolExecution,
                },
                message: failure.message().to_owned(),
            },
        },
        SessionEntryKind::TurnTerminated { turn_id, outcome } => {
            let (outcome, reason) = match outcome {
                TurnOutcome::Succeeded => (OutcomeRecord::Succeeded, None),
                TurnOutcome::Failed => (OutcomeRecord::Failed, None),
                TurnOutcome::Cancelled { reason } => {
                    (OutcomeRecord::Cancelled, Some(encode_reason(*reason)))
                }
            };
            KindRecord::TurnTerminated {
                turn_id: turn_id.as_str().to_owned(),
                outcome,
                reason,
            }
        }
        SessionEntryKind::OperationSettled {
            operation_id,
            outcome,
        } => {
            let (outcome, reason) = match outcome {
                OperationOutcome::Succeeded => (OutcomeRecord::Succeeded, None),
                OperationOutcome::Failed => (OutcomeRecord::Failed, None),
                OperationOutcome::Cancelled { reason } => {
                    (OutcomeRecord::Cancelled, Some(encode_reason(*reason)))
                }
            };
            KindRecord::OperationSettled {
                operation_id: operation_id.as_str().to_owned(),
                outcome,
                reason,
            }
        }
    }
}

fn decode_kind(
    record: KindRecord,
    load_artifact: &mut ArtifactLoader<'_>,
) -> Result<SessionEntryKind, String> {
    Ok(match record {
        KindRecord::OperationStarted { operation_id } => SessionEntryKind::OperationStarted {
            operation_id: OperationId::new(operation_id),
        },
        KindRecord::TurnStarted {
            operation_id,
            turn_id,
        } => SessionEntryKind::TurnStarted {
            operation_id: OperationId::new(operation_id),
            turn_id: TurnId::new(turn_id),
        },
        KindRecord::UserMessage {
            turn_id,
            content,
            parts,
        } => {
            let parts = match (parts, content) {
                (Some(parts), None) => parts
                    .into_iter()
                    .map(|part| match part {
                        UserPartRecord::Text { text } => Ok(SessionUserPart::Text(text)),
                        UserPartRecord::Image {
                            media_type,
                            artifact,
                            len,
                        } => Ok(SessionUserPart::Image {
                            media_type,
                            bytes: load_artifact(&artifact, len)?,
                        }),
                    })
                    .collect::<Result<Vec<_>, String>>()?,
                // Legacy pre-M8 shape: a plain content string is one text part.
                (None, Some(content)) => SessionUserPart::text_parts(content),
                _ => {
                    return Err(
                        "user_message record must have exactly one of parts or content".to_owned(),
                    );
                }
            };
            SessionEntryKind::UserMessage {
                turn_id: TurnId::new(turn_id),
                parts,
            }
        }
        KindRecord::AssistantToolCallBatch {
            turn_id,
            model_call_id,
            tool_batch_id,
            calls,
        } => SessionEntryKind::AssistantToolCallBatch {
            turn_id: TurnId::new(turn_id),
            model_call_id,
            tool_batch_id: ToolBatchId::new(tool_batch_id),
            calls: calls
                .into_iter()
                .map(|call| {
                    SessionToolCall::new(ToolCallId::new(call.id), call.name, call.arguments)
                })
                .collect(),
        },
        KindRecord::ToolResult {
            turn_id,
            tool_batch_id,
            result,
        } => SessionEntryKind::ToolResult {
            turn_id: TurnId::new(turn_id),
            tool_batch_id: ToolBatchId::new(tool_batch_id),
            result: match result.outcome {
                ToolOutcomeRecord::Success { content } => {
                    SessionToolResult::success(ToolCallId::new(result.call_id), content)
                }
                ToolOutcomeRecord::Error { code, message } => {
                    SessionToolResult::error(ToolCallId::new(result.call_id), code, message)
                }
                ToolOutcomeRecord::Cancelled => {
                    SessionToolResult::cancelled(ToolCallId::new(result.call_id))
                }
                ToolOutcomeRecord::Interrupted => {
                    SessionToolResult::interrupted(ToolCallId::new(result.call_id))
                }
            },
        },
        KindRecord::AssistantMessage { turn_id, content } => SessionEntryKind::AssistantMessage {
            turn_id: TurnId::new(turn_id),
            content,
        },
        KindRecord::TurnFailure { turn_id, failure } => SessionEntryKind::TurnFailure {
            turn_id: TurnId::new(turn_id),
            failure: TurnFailure::new(
                match failure.kind {
                    FailureKindRecord::ModelCall => TurnFailureKind::ModelCall,
                    FailureKindRecord::InvalidModelOutput => TurnFailureKind::InvalidModelOutput,
                    FailureKindRecord::Persistence => TurnFailureKind::Persistence,
                    FailureKindRecord::RuntimeDriver => TurnFailureKind::RuntimeDriver,
                    FailureKindRecord::ToolExecution => TurnFailureKind::ToolExecution,
                },
                failure.message,
            ),
        },
        KindRecord::TurnTerminated {
            turn_id,
            outcome,
            reason,
        } => SessionEntryKind::TurnTerminated {
            turn_id: TurnId::new(turn_id),
            outcome: match outcome {
                OutcomeRecord::Succeeded => TurnOutcome::Succeeded,
                OutcomeRecord::Failed => TurnOutcome::Failed,
                OutcomeRecord::Cancelled => TurnOutcome::Cancelled {
                    reason: decode_reason(reason),
                },
            },
        },
        KindRecord::OperationSettled {
            operation_id,
            outcome,
            reason,
        } => SessionEntryKind::OperationSettled {
            operation_id: OperationId::new(operation_id),
            outcome: match outcome {
                OutcomeRecord::Succeeded => OperationOutcome::Succeeded,
                OutcomeRecord::Failed => OperationOutcome::Failed,
                OutcomeRecord::Cancelled => OperationOutcome::Cancelled {
                    reason: decode_reason(reason),
                },
            },
        },
    })
}
