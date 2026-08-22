//! Maps Session / Runtime / Tools types onto frontend DTOs.

use philo_agent_runtime::{
    AgentAvailability, AgentEvent, ReasoningEffort, TokenUsage, UserMessage, UserPart,
};
use philo_session::{
    ContextMessage, SessionAssistantBlock, SessionContextView, SessionUserPart, ToolResultOutcome,
};
use philo_tools::{EffectClass, ToolDefinition, ToolDisplay, ToolResult};

use crate::frontend::command::{FrontendAttachment, FrontendReasoningEffort};
use crate::frontend::snapshot::{
    DurableSessionView, FrontendAssistantBlock, FrontendAvailability, FrontendConfigEntry,
    FrontendContextMessage, FrontendGeneration, FrontendOpenTurn, FrontendOperationEvent,
    FrontendStatus, FrontendTokenUsage, FrontendToolDisplay, FrontendToolListing,
    FrontendToolResult, FrontendToolResultOutcome, FrontendUnfilledBatch, FrontendUserPart,
};
use crate::live::LiveOperationSnapshot;
use philo_agent_runtime::RuntimeGeneration;

/// Maps a store view into a frontend DTO. Does not wrap or paginate.
pub fn durable_session_view(view: &SessionContextView) -> DurableSessionView {
    DurableSessionView {
        session_id: view.session_id().as_str().to_owned(),
        revision: view.revision().get(),
        messages: view.messages().iter().map(context_message).collect(),
        open_turns: view
            .open_turns()
            .iter()
            .map(|turn| FrontendOpenTurn {
                operation_id: turn.operation_id().as_str().to_owned(),
                turn_id: turn.turn_id().as_str().to_owned(),
                unfilled_batch: turn.unfilled_batch().map(|batch| FrontendUnfilledBatch {
                    tool_batch_id: batch.tool_batch_id().as_str().to_owned(),
                    unfilled_call_ids: batch
                        .unfilled_call_ids()
                        .iter()
                        .map(|id| id.as_str().to_owned())
                        .collect(),
                }),
            })
            .collect(),
        settled_turn_boundaries: view
            .settled_turn_boundaries()
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect(),
        latest_compaction_boundary: view
            .latest_compaction_boundary()
            .map(|id| id.as_str().to_owned()),
    }
}

/// True when the durable session view already contains a terminal for the live turn.
pub fn session_view_covers_live(view: &SessionContextView, live: &LiveOperationSnapshot) -> bool {
    let Some(operation_id) = live.operation_id.as_deref() else {
        return false;
    };
    view.settled_turns()
        .iter()
        .any(|(settled_operation, settled_turn)| {
            settled_operation.as_str() == operation_id
                && live
                    .turn_id
                    .as_deref()
                    .is_none_or(|turn_id| settled_turn.as_str() == turn_id)
        })
}

fn context_message(message: &ContextMessage) -> FrontendContextMessage {
    match message {
        ContextMessage::Summary { text } => FrontendContextMessage::Summary { text: text.clone() },
        ContextMessage::User { parts } => FrontendContextMessage::User {
            parts: parts.iter().map(user_part).collect(),
        },
        ContextMessage::Assistant { blocks } => FrontendContextMessage::Assistant {
            blocks: blocks.iter().map(assistant_block).collect(),
        },
        ContextMessage::AssistantToolCalls {
            tool_batch_id,
            blocks,
        } => FrontendContextMessage::AssistantToolCalls {
            tool_batch_id: tool_batch_id.as_str().to_owned(),
            blocks: blocks.iter().map(assistant_block).collect(),
        },
        ContextMessage::ToolResult {
            tool_call_id,
            outcome,
        } => FrontendContextMessage::ToolResult {
            tool_call_id: tool_call_id.as_str().to_owned(),
            outcome: tool_outcome(outcome),
        },
    }
}

fn user_part(part: &SessionUserPart) -> FrontendUserPart {
    match part {
        SessionUserPart::Text(text) => FrontendUserPart::Text(text.clone()),
        SessionUserPart::Image { media_type, bytes } => FrontendUserPart::Image {
            media_type: media_type.clone(),
            bytes: bytes.clone(),
        },
    }
}

fn assistant_block(block: &SessionAssistantBlock) -> FrontendAssistantBlock {
    match block {
        SessionAssistantBlock::Text { text } => FrontendAssistantBlock::Text { text: text.clone() },
        SessionAssistantBlock::ToolCall(call) => FrontendAssistantBlock::ToolCall {
            id: call.id().as_str().to_owned(),
            name: call.name().to_owned(),
            arguments: call.arguments().to_owned(),
        },
    }
}

fn tool_outcome(outcome: &ToolResultOutcome) -> FrontendToolResultOutcome {
    match outcome {
        ToolResultOutcome::Success { content } => FrontendToolResultOutcome::Success {
            content: content.clone(),
        },
        ToolResultOutcome::Error { code, message } => FrontendToolResultOutcome::Error {
            code: code.clone(),
            message: message.clone(),
        },
        ToolResultOutcome::Cancelled => FrontendToolResultOutcome::Cancelled,
        ToolResultOutcome::Interrupted => FrontendToolResultOutcome::Interrupted,
    }
}

/// Maps a runtime agent event onto a frontend DTO. Unknown variants are skipped.
pub fn operation_event(event: &AgentEvent) -> Option<FrontendOperationEvent> {
    Some(match event {
        AgentEvent::OperationQueued { operation_id } => FrontendOperationEvent::OperationQueued {
            operation_id: operation_id.as_str().to_owned(),
        },
        AgentEvent::OperationStarted { operation_id } => FrontendOperationEvent::OperationStarted {
            operation_id: operation_id.as_str().to_owned(),
        },
        AgentEvent::TurnStarted { turn_id } => FrontendOperationEvent::TurnStarted {
            turn_id: turn_id.as_str().to_owned(),
        },
        AgentEvent::ModelCallStarted { model_call_id } => {
            FrontendOperationEvent::ModelCallStarted {
                model_call_id: model_call_id.as_str().to_owned(),
            }
        }
        AgentEvent::ModelResponseStarted {
            model_call_id,
            response_model,
            response_id,
        } => FrontendOperationEvent::ModelResponseStarted {
            model_call_id: model_call_id.as_str().to_owned(),
            response_model: response_model.clone(),
            response_id: response_id.clone(),
        },
        AgentEvent::TextDelta { delta } => FrontendOperationEvent::TextDelta {
            delta: delta.clone(),
        },
        AgentEvent::ReasoningDelta {
            model_call_id,
            text,
        } => FrontendOperationEvent::ReasoningDelta {
            model_call_id: model_call_id.as_str().to_owned(),
            text: text.clone(),
        },
        AgentEvent::ModelUsageUpdated {
            model_call_id,
            usage,
        } => FrontendOperationEvent::ModelUsageUpdated {
            model_call_id: model_call_id.as_str().to_owned(),
            usage: token_usage(*usage),
        },
        AgentEvent::ToolBatchRequested {
            tool_batch_id,
            call_count,
        } => FrontendOperationEvent::ToolBatchRequested {
            tool_batch_id: tool_batch_id.as_str().to_owned(),
            call_count: *call_count,
        },
        AgentEvent::ToolExecutionStarted {
            tool_batch_id,
            tool_call_id,
            index,
            tool_name,
            arguments,
        } => FrontendOperationEvent::ToolExecutionStarted {
            tool_batch_id: tool_batch_id.as_str().to_owned(),
            tool_call_id: tool_call_id.as_str().to_owned(),
            index: *index,
            tool_name: tool_name.clone(),
            arguments: arguments.clone(),
        },
        AgentEvent::ToolExecutionProgress {
            tool_batch_id,
            tool_call_id,
            index,
            tail,
        } => FrontendOperationEvent::ToolExecutionProgress {
            tool_batch_id: tool_batch_id.as_str().to_owned(),
            tool_call_id: tool_call_id.as_str().to_owned(),
            index: *index,
            tail: tail.clone(),
        },
        AgentEvent::ToolExecutionCompleted {
            tool_batch_id,
            tool_call_id,
            index,
            tool_name,
            result,
            display,
        } => FrontendOperationEvent::ToolExecutionCompleted {
            tool_batch_id: tool_batch_id.as_str().to_owned(),
            tool_call_id: tool_call_id.as_str().to_owned(),
            index: *index,
            tool_name: tool_name.clone(),
            result: tool_result(result),
            display: display.as_ref().map(tool_display),
        },
        AgentEvent::AssistantMessageCompleted { turn_id, message } => {
            FrontendOperationEvent::AssistantMessageCompleted {
                turn_id: turn_id.as_str().to_owned(),
                content: message.content().to_owned(),
            }
        }
        AgentEvent::TurnFailed { turn_id, failure } => FrontendOperationEvent::TurnFailed {
            turn_id: turn_id.as_str().to_owned(),
            kind: format!("{:?}", failure.kind()),
            message: failure.message().to_owned(),
        },
        AgentEvent::PriorTurnSealed { turn_id } => FrontendOperationEvent::PriorTurnSealed {
            turn_id: turn_id.as_str().to_owned(),
        },
        AgentEvent::ContextCompactionStarted => FrontendOperationEvent::ContextCompactionStarted,
        AgentEvent::ContextCompactionCompleted { covers_up_to } => {
            FrontendOperationEvent::ContextCompactionCompleted {
                covers_up_to: covers_up_to.clone(),
            }
        }
        AgentEvent::ContextCompactionFailed { message } => {
            FrontendOperationEvent::ContextCompactionFailed {
                message: message.clone(),
            }
        }
        AgentEvent::CancellationRequested {
            operation_id,
            reason,
        } => FrontendOperationEvent::CancellationRequested {
            operation_id: operation_id.as_str().to_owned(),
            reason: format!("{reason:?}"),
        },
        AgentEvent::TurnCancelled { turn_id, reason } => FrontendOperationEvent::TurnCancelled {
            turn_id: turn_id.as_str().to_owned(),
            reason: format!("{reason:?}"),
        },
        AgentEvent::ModelRetryScheduled {
            model_call_id,
            attempt,
            max_retries,
            delay_ms,
            reason,
        } => FrontendOperationEvent::ModelRetryScheduled {
            model_call_id: model_call_id.as_str().to_owned(),
            attempt: *attempt,
            max_retries: *max_retries,
            delay_ms: *delay_ms,
            reason: reason.clone(),
        },
        _ => return None,
    })
}

fn tool_result(result: &ToolResult) -> FrontendToolResult {
    match result {
        ToolResult::Success { content } => FrontendToolResult::Success {
            content: content.clone(),
        },
        ToolResult::Error { code, message } => FrontendToolResult::Error {
            code: code.clone(),
            message: message.clone(),
        },
    }
}

fn tool_display(display: &ToolDisplay) -> FrontendToolDisplay {
    FrontendToolDisplay {
        detail: display.detail().to_owned(),
        facts: display
            .facts()
            .iter()
            .map(|fact| (fact.name().to_owned(), fact.value().to_owned()))
            .collect(),
    }
}

/// Maps runtime availability onto a frontend DTO.
pub fn availability(value: &AgentAvailability) -> FrontendAvailability {
    match value {
        AgentAvailability::Idle => FrontendAvailability::Idle,
        AgentAvailability::Busy { operation_id } => FrontendAvailability::Busy {
            operation_id: operation_id.as_str().to_owned(),
        },
        AgentAvailability::Compacting { session_id } => FrontendAvailability::Compacting {
            session_id: session_id.as_str().to_owned(),
        },
    }
}

/// Maps observed token usage onto a frontend DTO.
pub fn token_usage(usage: TokenUsage) -> FrontendTokenUsage {
    FrontendTokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        reasoning_tokens: usage.reasoning_tokens,
    }
}

/// Builds a [`UserMessage`] from a submit command.
pub fn user_message(
    draft: &str,
    attachments: &[FrontendAttachment],
) -> Result<UserMessage, String> {
    let mut parts = Vec::new();
    if !draft.is_empty() {
        parts.push(UserPart::Text(draft.to_owned()));
    }
    for attachment in attachments {
        parts.push(UserPart::Image {
            media_type: attachment.media_type.clone(),
            bytes: attachment.bytes.clone(),
        });
    }
    UserMessage::from_parts(parts).map_err(|error| format!("{error:?}"))
}

/// Maps a frontend reasoning DTO onto the runtime vocabulary.
pub fn reasoning_effort(effort: FrontendReasoningEffort) -> ReasoningEffort {
    match effort {
        FrontendReasoningEffort::Minimal => ReasoningEffort::Minimal,
        FrontendReasoningEffort::Low => ReasoningEffort::Low,
        FrontendReasoningEffort::Medium => ReasoningEffort::Medium,
        FrontendReasoningEffort::High => ReasoningEffort::High,
        FrontendReasoningEffort::Xhigh => ReasoningEffort::Xhigh,
        FrontendReasoningEffort::Max => ReasoningEffort::Max,
    }
}

/// Secret-free config entries derived from the current generation.
pub fn config_entries(generation: &RuntimeGeneration) -> Vec<FrontendConfigEntry> {
    let config = &generation.runtime_config;
    vec![
        entry("model", &generation.display.model_name, "generation"),
        entry(
            "reasoning",
            generation
                .runtime_config
                .generation
                .reasoning_effort
                .map(|effort| format!("{effort:?}"))
                .unwrap_or_else(|| "default".to_owned()),
            "generation",
        ),
        entry(
            "max_tool_rounds",
            config.max_tool_rounds.to_string(),
            "generation",
        ),
        entry(
            "max_parallel_tool_calls",
            config.max_parallel_tool_calls.to_string(),
            "generation",
        ),
        entry(
            "operation_timeout",
            config
                .operation_timeout
                .map(|timeout| format!("{}ms", timeout.as_millis()))
                .unwrap_or_else(|| "none".to_owned()),
            "generation",
        ),
    ]
}

fn entry(key: &str, value: impl Into<String>, source: &str) -> FrontendConfigEntry {
    FrontendConfigEntry {
        key: key.to_owned(),
        value: value.into(),
        source: source.to_owned(),
    }
}

/// `/status` payload from the current generation and availability.
pub fn status(
    generation: &RuntimeGeneration,
    availability: FrontendAvailability,
    queued: usize,
) -> FrontendStatus {
    FrontendStatus {
        availability,
        queued,
        generation: frontend_generation(generation),
        tools: generation
            .tools
            .definitions()
            .into_iter()
            .map(tool_listing)
            .collect(),
    }
}

fn tool_listing(definition: ToolDefinition) -> FrontendToolListing {
    FrontendToolListing {
        name: definition.name().to_owned(),
        effect_class: effect_class(definition.effect_class()),
    }
}

fn effect_class(class: EffectClass) -> String {
    match class {
        EffectClass::ReadOnly => "read_only".to_owned(),
        EffectClass::Workspace => "workspace".to_owned(),
        EffectClass::System => "system".to_owned(),
    }
}

/// Maps a frozen generation onto a frontend DTO.
pub fn frontend_generation(generation: &RuntimeGeneration) -> FrontendGeneration {
    FrontendGeneration {
        generation_id: generation.generation_id.to_string(),
        model_name: generation.display.model_name.clone(),
        reasoning_effort: generation
            .runtime_config
            .generation
            .reasoning_effort
            .map(|effort| format!("{effort:?}")),
        tool_names: generation
            .tools
            .definitions()
            .into_iter()
            .map(|tool| tool.name().to_owned())
            .collect(),
    }
}

/// Converts a frontend session id into the session-store identifier.
pub fn session_store_id(session_id: &str) -> philo_session::SessionId {
    philo_session::SessionId::new(session_id)
}

/// Converts a frontend session id into the runtime identifier.
pub fn session_runtime_id(session_id: &str) -> philo_agent_runtime::SessionId {
    philo_agent_runtime::SessionId::new(session_id)
}

/// Converts a frontend operation id into the runtime identifier.
pub fn operation_runtime_id(operation_id: &str) -> philo_agent_runtime::OperationId {
    philo_agent_runtime::OperationId::new(operation_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use philo_session::{
        MemorySessionStore, OperationId, OperationOutcome, SessionAssistantBlock, SessionEntryKind,
        SessionRevision, SessionStore, SessionTransaction, SessionUserPart, TurnId, TurnOutcome,
    };

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(value) => value,
            std::task::Poll::Pending => panic!("memory store future unexpectedly pending"),
        }
    }

    #[test]
    fn durable_view_matches_store_message_count() {
        let store = MemorySessionStore::new();
        let session_id = philo_session::SessionId::new("map-session");
        block_on(store.commit(SessionTransaction::linear(
            session_id.clone(),
            SessionRevision::ZERO,
            vec![
                SessionEntryKind::OperationStarted {
                    operation_id: OperationId::new("op-1"),
                },
                SessionEntryKind::TurnStarted {
                    operation_id: OperationId::new("op-1"),
                    turn_id: TurnId::new("turn-1"),
                },
                SessionEntryKind::UserMessage {
                    turn_id: TurnId::new("turn-1"),
                    parts: SessionUserPart::text_parts("hello"),
                },
                SessionEntryKind::AssistantMessage {
                    turn_id: TurnId::new("turn-1"),
                    blocks: vec![SessionAssistantBlock::Text {
                        text: "world".into(),
                    }],
                },
                SessionEntryKind::TurnTerminated {
                    turn_id: TurnId::new("turn-1"),
                    outcome: TurnOutcome::Succeeded,
                },
                SessionEntryKind::OperationSettled {
                    operation_id: OperationId::new("op-1"),
                    outcome: OperationOutcome::Succeeded,
                },
            ],
        )))
        .unwrap();
        let view = block_on(store.context_view(&session_id)).unwrap();
        let dto = durable_session_view(&view);
        assert_eq!(dto.messages.len(), view.messages().len());
        assert_eq!(dto.messages.len(), 2);
        assert_eq!(view.settled_turns().len(), 1);
        assert_eq!(view.settled_turns()[0].0.as_str(), "op-1");
        assert_eq!(view.settled_turns()[0].1.as_str(), "turn-1");
        let mut live = crate::live::LiveOperationSnapshot::new();
        live.accept("op-1", "turn-1");
        live.push_text("world");
        assert!(session_view_covers_live(&view, &live));
        assert!(matches!(
            &dto.messages[0],
            FrontendContextMessage::User { .. }
        ));
    }
}
