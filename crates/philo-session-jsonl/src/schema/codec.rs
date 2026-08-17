//! Explicit conversion between schema v2 records and session domain facts.

use philo_session::{
    CancelReason, EntryId, OperationId, OperationOutcome, SessionAssistantBlock, SessionEntry,
    SessionEntryKind, SessionToolCall, SessionToolResult, SessionUserPart, ToolBatchId, ToolCallId,
    ToolResultOutcome, TurnFailure, TurnFailureKind, TurnId, TurnOutcome,
};

use crate::artifact::sha256_hex;

use super::record::{
    AssistantBlockRecord, AssistantMessageRecord, AssistantToolCallBatchRecord, EntryRecord,
    FailureKindRecord, FailureRecord, KindRecord, OutcomeRecord, ReasonRecord, ToolOutcomeRecord,
    ToolResultRecord, UserMessageRecord, UserPartRecord,
};

/// An artifact newly referenced by the transaction being encoded.
#[derive(Debug)]
pub(crate) struct PendingArtifact {
    pub hash: String,
    pub bytes: Vec<u8>,
}

type ArtifactLoader<'a> = dyn FnMut(&str, u64) -> Result<Vec<u8>, String> + 'a;

fn encode_reason(reason: CancelReason) -> ReasonRecord {
    match reason {
        CancelReason::User => ReasonRecord::User,
        CancelReason::Timeout => ReasonRecord::Timeout,
        CancelReason::Abandoned => ReasonRecord::Abandoned,
    }
}

fn decode_reason(reason: Option<ReasonRecord>) -> Result<CancelReason, String> {
    match reason {
        Some(ReasonRecord::User) => Ok(CancelReason::User),
        Some(ReasonRecord::Timeout) => Ok(CancelReason::Timeout),
        Some(ReasonRecord::Abandoned) => Ok(CancelReason::Abandoned),
        None => Err("cancelled outcome is missing required reason".to_owned()),
    }
}

fn encode_block(block: &SessionAssistantBlock) -> AssistantBlockRecord {
    match block {
        SessionAssistantBlock::Text { text } => AssistantBlockRecord::Text { text: text.clone() },
        SessionAssistantBlock::ToolCall(call) => AssistantBlockRecord::ToolCall {
            id: call.id().as_str().to_owned(),
            name: call.name().to_owned(),
            arguments: call.arguments().to_owned(),
        },
    }
}

fn decode_block(block: AssistantBlockRecord) -> SessionAssistantBlock {
    match block {
        AssistantBlockRecord::Text { text } => SessionAssistantBlock::Text { text },
        AssistantBlockRecord::ToolCall {
            id,
            name,
            arguments,
        } => SessionAssistantBlock::ToolCall(SessionToolCall::new(
            ToolCallId::new(id),
            name,
            arguments,
        )),
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

/// Decodes one persisted entry, resolving image references through the
/// supplied artifact loader.
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
        SessionEntryKind::UserMessage { turn_id, parts } => {
            KindRecord::UserMessage(UserMessageRecord {
                turn_id: turn_id.as_str().to_owned(),
                parts: parts
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
            })
        }
        SessionEntryKind::AssistantToolCallBatch {
            turn_id,
            model_call_id,
            tool_batch_id,
            blocks,
        } => KindRecord::AssistantToolCallBatch(AssistantToolCallBatchRecord {
            turn_id: turn_id.as_str().to_owned(),
            model_call_id: model_call_id.clone(),
            tool_batch_id: tool_batch_id.as_str().to_owned(),
            blocks: blocks.iter().map(encode_block).collect(),
        }),
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
        SessionEntryKind::AssistantMessage { turn_id, blocks } => {
            KindRecord::AssistantMessage(AssistantMessageRecord {
                turn_id: turn_id.as_str().to_owned(),
                blocks: blocks.iter().map(encode_block).collect(),
            })
        }
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
        SessionEntryKind::Compaction {
            summary,
            covers_up_to,
        } => KindRecord::Compaction {
            summary: summary.clone(),
            covers_up_to: covers_up_to.as_str().to_owned(),
        },
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
        KindRecord::UserMessage(UserMessageRecord { turn_id, parts }) => {
            SessionEntryKind::UserMessage {
                turn_id: TurnId::new(turn_id),
                parts: parts
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
            }
        }
        KindRecord::AssistantToolCallBatch(AssistantToolCallBatchRecord {
            turn_id,
            model_call_id,
            tool_batch_id,
            blocks,
        }) => SessionEntryKind::AssistantToolCallBatch {
            turn_id: TurnId::new(turn_id),
            model_call_id,
            tool_batch_id: ToolBatchId::new(tool_batch_id),
            blocks: blocks.into_iter().map(decode_block).collect(),
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
        KindRecord::AssistantMessage(AssistantMessageRecord { turn_id, blocks }) => {
            SessionEntryKind::AssistantMessage {
                turn_id: TurnId::new(turn_id),
                blocks: blocks.into_iter().map(decode_block).collect(),
            }
        }
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
                    reason: decode_reason(reason)?,
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
                    reason: decode_reason(reason)?,
                },
            },
        },
        KindRecord::Compaction {
            summary,
            covers_up_to,
        } => SessionEntryKind::Compaction {
            summary,
            covers_up_to: EntryId::new(covers_up_to),
        },
    })
}
