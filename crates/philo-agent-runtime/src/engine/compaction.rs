//! M13 context compaction: policy, summary model call, and atomic commit.

use super::EngineContext;
use super::seal;
use super::stream::{StreamStep, next_or_cancel};
use crate::mapping::failure::session_failure;
use crate::mapping::messages::context_messages;
use crate::operation::OperationPublisher;
use crate::{
    AgentEvent, CompactionError, CompactionReport, ModelCallId, ModelCallSnapshot, ModelEvent,
    ModelMessage, OperationId, SessionId, TurnId, UserPart,
};
use philo_session as session;

const SUMMARY_INSTRUCTION: &str = "Summarize the earlier conversation for a continuation. Preserve decisions, constraints, unresolved work, and concrete identifiers. Return only the summary text.";

pub(super) enum AutoCompactionOutcome {
    Ready(OperationPublisher, session::SessionContextView),
    Settled,
}

pub(super) async fn maybe_auto_compact(
    ctx: &EngineContext,
    operation: OperationPublisher,
    runtime_session_id: &SessionId,
    stored_session_id: &session::SessionId,
    context: session::SessionContextView,
) -> AutoCompactionOutcome {
    if !should_auto_compact(ctx, runtime_session_id, &context) {
        return AutoCompactionOutcome::Ready(operation, context);
    }
    let Some(covers_up_to) = select_boundary(&context, ctx.config.compaction.keep_recent_turns)
    else {
        return AutoCompactionOutcome::Ready(operation, context);
    };

    operation.push(AgentEvent::ContextCompactionStarted);
    let summary = match generate_summary(ctx, Some(&operation), runtime_session_id, &context).await
    {
        Ok(summary) => summary,
        Err(SummaryFailure::Cancelled) => {
            operation.cancellation_observed();
            operation.cancel_zero_trace();
            return AutoCompactionOutcome::Settled;
        }
        Err(SummaryFailure::Failed(error)) => {
            operation.push(AgentEvent::ContextCompactionFailed {
                message: error.message().to_owned(),
            });
            return AutoCompactionOutcome::Ready(operation, context);
        }
    };

    if operation.is_cancel_requested() {
        operation.cancellation_observed();
        operation.cancel_zero_trace();
        return AutoCompactionOutcome::Settled;
    }
    let commit = ctx
        .sessions
        .commit(session::SessionTransaction::linear(
            stored_session_id.clone(),
            context.revision(),
            vec![session::SessionEntryKind::Compaction {
                summary,
                covers_up_to: covers_up_to.clone(),
            }],
        ))
        .await;
    if let Err(error) = commit {
        operation.push(AgentEvent::ContextCompactionFailed {
            message: format!("committing context compaction: {error:?}"),
        });
        return AutoCompactionOutcome::Ready(operation, context);
    }

    operation.push(AgentEvent::ContextCompactionCompleted {
        covers_up_to: covers_up_to.as_str().to_owned(),
    });
    // A cancellation that arrives after the atomic maintenance commit keeps
    // that useful fact but still prevents Barrier A from creating a turn.
    if operation.is_cancel_requested() {
        operation.cancellation_observed();
        operation.cancel_zero_trace();
        return AutoCompactionOutcome::Settled;
    }
    match ctx.sessions.context_view(stored_session_id).await {
        Ok(context) => AutoCompactionOutcome::Ready(operation, context),
        Err(error) => {
            operation.fail_unconfirmed(session_failure(
                "re-reading compacted session context",
                &error,
            ));
            AutoCompactionOutcome::Settled
        }
    }
}

pub(crate) async fn compact_manually(
    ctx: &EngineContext,
    runtime_session_id: &SessionId,
) -> Result<CompactionReport, CompactionError> {
    let stored_session_id = session::SessionId::new(runtime_session_id.as_str());
    let context = ctx
        .sessions
        .context_view(&stored_session_id)
        .await
        .map_err(|error| session_error("reading session context", error))?;
    let context = seal::seal_stale_turns_for_maintenance(ctx, &stored_session_id, context)
        .await
        .map_err(|failure| match failure {
            seal::SealFailure::Commit(error) => session_error("sealing stale turn", error),
            seal::SealFailure::Refresh(error) => session_error("re-reading sealed context", error),
        })?;
    let Some(covers_up_to) = select_boundary(&context, ctx.config.compaction.keep_recent_turns)
    else {
        return Ok(CompactionReport::NothingToCompact);
    };
    let summary = generate_summary(ctx, None, runtime_session_id, &context)
        .await
        .map_err(|failure| match failure {
            SummaryFailure::Failed(error) => error,
            SummaryFailure::Cancelled => {
                unreachable!("manual compaction has no operation cancellation source")
            }
        })?;
    ctx.sessions
        .commit(session::SessionTransaction::linear(
            stored_session_id,
            context.revision(),
            vec![session::SessionEntryKind::Compaction {
                summary,
                covers_up_to: covers_up_to.clone(),
            }],
        ))
        .await
        .map_err(|error| session_error("committing context compaction", error))?;
    Ok(CompactionReport::Compacted {
        covers_up_to: covers_up_to.as_str().to_owned(),
    })
}

fn should_auto_compact(
    ctx: &EngineContext,
    session_id: &SessionId,
    context: &session::SessionContextView,
) -> bool {
    let Some(budget) = ctx.config.compaction.context_budget else {
        return false;
    };
    let observed = ctx.last_input_tokens(session_id).unwrap_or_else(|| {
        let bytes = context_bytes(context);
        bytes / u64::from(ctx.config.compaction.estimate_bytes_per_token.max(1))
    });
    (observed as f32) >= ctx.config.compaction.auto_threshold * (budget as f32)
}

fn select_boundary(
    context: &session::SessionContextView,
    keep_recent_turns: u32,
) -> Option<session::EntryId> {
    let boundaries = context.settled_turn_boundaries();
    let keep = usize::try_from(keep_recent_turns).unwrap_or(usize::MAX);
    if boundaries.len() <= keep {
        return None;
    }
    let candidate_index = boundaries.len() - keep - 1;
    if let Some(previous) = context.latest_compaction_boundary() {
        let previous_index = boundaries.iter().position(|entry| entry == previous)?;
        if candidate_index <= previous_index {
            return None;
        }
    }
    Some(boundaries[candidate_index].clone())
}

enum SummaryFailure {
    Cancelled,
    Failed(CompactionError),
}

async fn generate_summary(
    ctx: &EngineContext,
    operation: Option<&OperationPublisher>,
    session_id: &SessionId,
    context: &session::SessionContextView,
) -> Result<String, SummaryFailure> {
    if operation.is_some_and(OperationPublisher::is_cancel_requested) {
        return Err(SummaryFailure::Cancelled);
    }
    let synthetic_prefix = format!(
        "compaction:{}:revision:{}",
        session_id.as_str(),
        context.revision().get()
    );
    let mut messages = vec![ModelMessage::System {
        content: ctx.config.system_prompt.clone(),
    }];
    messages.extend(context_messages(context));
    messages.push(ModelMessage::User {
        parts: vec![UserPart::Text(SUMMARY_INSTRUCTION.to_owned())],
    });
    let request = ModelCallSnapshot {
        session_id: session_id.clone(),
        context_fingerprint: context
            .current_leaf()
            .map_or_else(|| "empty".to_owned(), |leaf| leaf.as_str().to_owned()),
        persist_replay: false,
        operation_id: OperationId::new(format!("{synthetic_prefix}:operation")),
        turn_id: TurnId::new(format!("{synthetic_prefix}:turn")),
        model_call_id: ModelCallId::new(format!("{synthetic_prefix}:call")),
        model_call_index: 1,
        session_revision: context.revision(),
        messages,
        tools: Vec::new(),
        model_target: ctx.config.model_target.clone(),
        generation: ctx.config.generation.clone(),
    };
    let mut stream = ctx.model.start(request).await.map_err(|error| {
        SummaryFailure::Failed(CompactionError::Model {
            message: error.message().to_owned(),
        })
    })?;
    let mut summary = String::new();
    let mut completed = false;
    let mut response_started = false;
    loop {
        let step = match operation {
            Some(operation) => next_or_cancel(stream.as_mut(), operation.shared()).await,
            None => StreamStep::Event(stream.next().await),
        };
        match step {
            StreamStep::CancelObserved => {
                drop(stream);
                return Err(SummaryFailure::Cancelled);
            }
            StreamStep::Event(Some(Err(error))) => {
                return Err(SummaryFailure::Failed(CompactionError::Model {
                    message: error.message().to_owned(),
                }));
            }
            StreamStep::Event(Some(Ok(event))) if completed => {
                return Err(invalid_summary(match event {
                    ModelEvent::Completed => "summary stream emitted Completed more than once",
                    _ => "summary stream emitted output after Completed",
                }));
            }
            StreamStep::Event(Some(Ok(ModelEvent::Completed))) => completed = true,
            StreamStep::Event(Some(Ok(ModelEvent::TextDelta(delta)))) => {
                summary.push_str(&delta);
            }
            StreamStep::Event(Some(Ok(ModelEvent::ResponseStarted { .. }))) => {
                if response_started {
                    return Err(invalid_summary(
                        "summary stream emitted ResponseStarted more than once",
                    ));
                }
                response_started = true;
            }
            StreamStep::Event(Some(Ok(ModelEvent::ToolCallDelta { .. }))) => {
                return Err(invalid_summary(
                    "summary model returned tool calls instead of FinalText",
                ));
            }
            StreamStep::Event(Some(Ok(
                ModelEvent::ReasoningDelta { .. } | ModelEvent::UsageUpdated { .. },
            ))) => {}
            StreamStep::Event(None) if completed => break,
            StreamStep::Event(None) => {
                return Err(invalid_summary("summary stream ended before Completed"));
            }
        }
    }
    if summary.is_empty() {
        return Err(invalid_summary("summary model returned empty FinalText"));
    }
    Ok(summary)
}

fn invalid_summary(message: &str) -> SummaryFailure {
    SummaryFailure::Failed(CompactionError::InvalidModelOutput {
        message: message.to_owned(),
    })
}

fn session_error(context: &str, error: session::SessionError) -> CompactionError {
    CompactionError::Session {
        message: format!("{context}: {error:?}"),
    }
}

fn context_bytes(context: &session::SessionContextView) -> u64 {
    context.messages().iter().fold(0_u64, |total, message| {
        total.saturating_add(message_bytes(message))
    })
}

fn message_bytes(message: &session::ContextMessage) -> u64 {
    match message {
        session::ContextMessage::Summary { text }
        | session::ContextMessage::Assistant { content: text } => byte_len(text),
        session::ContextMessage::User { parts } => parts.iter().fold(0, |total, part| {
            total.saturating_add(match part {
                session::SessionUserPart::Text(text) => byte_len(text),
                session::SessionUserPart::Image { media_type, bytes } => {
                    byte_len(media_type).saturating_add(len_u64(bytes.len()))
                }
            })
        }),
        session::ContextMessage::AssistantToolCalls {
            tool_batch_id,
            calls,
        } => calls
            .iter()
            .fold(byte_len(tool_batch_id.as_str()), |total, call| {
                total
                    .saturating_add(byte_len(call.id().as_str()))
                    .saturating_add(byte_len(call.name()))
                    .saturating_add(byte_len(call.arguments()))
            }),
        session::ContextMessage::ToolResult {
            tool_call_id,
            outcome,
        } => {
            let payload = match outcome {
                session::ToolResultOutcome::Success { content } => byte_len(content),
                session::ToolResultOutcome::Error { code, message } => {
                    byte_len(code).saturating_add(byte_len(message))
                }
                session::ToolResultOutcome::Cancelled | session::ToolResultOutcome::Interrupted => {
                    0
                }
            };
            byte_len(tool_call_id.as_str()).saturating_add(payload)
        }
    }
}

fn byte_len(value: &str) -> u64 {
    len_u64(value.len())
}

fn len_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
