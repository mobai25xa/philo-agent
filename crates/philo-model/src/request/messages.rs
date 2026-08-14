use philo::api::stable as sdk;
use philo_agent_runtime::{ModelError, ModelMessage, ModelToolResultOutcome, UserPart};

use crate::replay::CachedReasoning;

/// Canonical instructions-channel marker for durable conversation summaries.
const SUMMARY_INSTRUCTION_PREFIX: &str = "Summary of earlier conversation:\n";

/// Model-visible placeholder for an empty tool success text: the SDK requires
/// every tool-result message to carry at least one non-empty text block.
const EMPTY_TOOL_RESULT_TEXT: &str = "(empty)";

/// Canonical, stable model-visible text replayed for a tool call that never
/// executed because its turn was cancelled.
const CANCELLED_TOOL_RESULT_TEXT: &str =
    "cancelled: the tool call did not execute because the turn was cancelled";

/// Canonical, stable model-visible text for a tool call whose process was
/// interrupted (M11): execution state unknown, side effects may have
/// happened. Shared by durable seal results and the runtime's synthesized
/// placeholders for dangling batches of terminated turns.
const INTERRUPTED_TOOL_RESULT_TEXT: &str = "interrupted: the process was interrupted \
     while this call was outstanding; whether it executed is unknown, so verify the actual state \
     before assuming";

/// Maps provider-neutral history while keeping instructions separate from
/// conversational messages. Summaries always follow the main system prompt
/// in the SDK instructions channel and never impersonate user input.
pub(super) fn map_messages(
    request: &mut sdk::ModelRequest,
    messages: &[ModelMessage],
    native_error_status: bool,
    replayed: &[Vec<CachedReasoning>],
) -> Result<(), ModelError> {
    // The trailing `replayed.len()` assistant tool-call messages belong to
    // the current turn: batch k was produced by logical call k.
    let total_batches = messages
        .iter()
        .filter(|message| matches!(message, ModelMessage::AssistantToolCalls { .. }))
        .count();
    let replay_offset = total_batches.saturating_sub(replayed.len());
    let mut batch_sequence = 0usize;

    let mut system_instructions = Vec::new();
    let mut summary_instructions = Vec::new();
    for message in messages {
        match message {
            ModelMessage::System { content } => system_instructions.push(content.as_str()),
            ModelMessage::Summary { text } => {
                summary_instructions.push(format!("{SUMMARY_INSTRUCTION_PREFIX}{text}"))
            }
            ModelMessage::User { parts } => request.messages.push(sdk::Message::User {
                content: map_user_parts(parts)?,
            }),
            ModelMessage::Assistant { content } => {
                request.messages.push(sdk::Message::Assistant {
                    content: vec![sdk::ResponseItem::new(
                        sdk::BlockId::new(0),
                        0,
                        sdk::AssistantContent::Text {
                            text: content.clone(),
                        },
                        sdk::ReplayRequirement::None,
                        None,
                    )],
                });
            }
            ModelMessage::AssistantToolCalls { calls } => {
                let reasoning_items: &[CachedReasoning] = if batch_sequence >= replay_offset {
                    replayed
                        .get(batch_sequence - replay_offset)
                        .map_or(&[], Vec::as_slice)
                } else {
                    &[]
                };
                batch_sequence += 1;
                request
                    .messages
                    .push(map_assistant_tool_calls(calls, reasoning_items)?);
            }
            ModelMessage::ToolResult {
                tool_call_id,
                outcome,
            } => request.messages.push(map_tool_result(
                tool_call_id.as_str(),
                outcome,
                native_error_status,
            )?),
        }
    }

    let mut instructions = system_instructions.join("\n\n");
    for summary in summary_instructions {
        if !instructions.is_empty() {
            instructions.push_str("\n\n");
        }
        instructions.push_str(&summary);
    }
    if !instructions.is_empty() {
        request.instructions = Some(instructions);
    }
    Ok(())
}

fn map_assistant_tool_calls(
    calls: &[philo_agent_runtime::ModelToolCall],
    reasoning_items: &[CachedReasoning],
) -> Result<sdk::Message, ModelError> {
    // Reasoning items replay first, then the tool calls, with contiguous
    // zero-based indices and response-unique block IDs.
    let mut items = Vec::with_capacity(reasoning_items.len() + calls.len());
    for reasoning in reasoning_items {
        let position = items.len();
        items.push(sdk::ResponseItem::new(
            sdk::BlockId::new(position as u64),
            u32::try_from(position).map_err(|_| history_error("reasoning index"))?,
            sdk::AssistantContent::Reasoning {
                kind: reasoning.kind,
                text: reasoning.text.clone(),
            },
            reasoning.replay_requirement,
            reasoning.replay_token.clone(),
        ));
    }
    for call in calls {
        let id = sdk::ToolCallId::new(call.tool_call_id.as_str())
            .map_err(|_| history_error("tool call id"))?;
        let name =
            sdk::ToolName::new(call.name.as_str()).map_err(|_| history_error("tool call name"))?;
        let tool_call = match sdk::ToolCall::from_raw(id.clone(), name.clone(), &call.arguments) {
            Ok(tool_call) => tool_call,
            // Replay degradation: invalid arguments replay as `{}`. Durable
            // facts are untouched; the paired error result keeps the detail.
            Err(_) => sdk::ToolCall::from_raw(id, name, "{}").expect("`{}` is a valid JSON object"),
        };
        let position = items.len();
        items.push(sdk::ResponseItem::new(
            sdk::BlockId::new(position as u64),
            u32::try_from(position).map_err(|_| history_error("tool call index"))?,
            sdk::AssistantContent::ToolCall(tool_call),
            sdk::ReplayRequirement::None,
            None,
        ));
    }
    Ok(sdk::Message::Assistant { content: items })
}

fn map_tool_result(
    tool_call_id: &str,
    outcome: &ModelToolResultOutcome,
    native_error_status: bool,
) -> Result<sdk::Message, ModelError> {
    let call_id =
        sdk::ToolCallId::new(tool_call_id).map_err(|_| history_error("tool result call id"))?;
    let error_status = if native_error_status {
        sdk::ToolResultStatus::Error
    } else {
        sdk::ToolResultStatus::Success
    };
    let (status, text) = match outcome {
        ModelToolResultOutcome::Success { content } => (
            sdk::ToolResultStatus::Success,
            if content.is_empty() {
                EMPTY_TOOL_RESULT_TEXT.to_owned()
            } else {
                content.clone()
            },
        ),
        ModelToolResultOutcome::Error { code, message } => {
            (error_status, format!("{code}: {message}"))
        }
        ModelToolResultOutcome::Cancelled => (error_status, CANCELLED_TOOL_RESULT_TEXT.to_owned()),
        ModelToolResultOutcome::Interrupted => {
            (error_status, INTERRUPTED_TOOL_RESULT_TEXT.to_owned())
        }
    };
    Ok(sdk::Message::ToolResult(sdk::ToolResultMessage {
        call_id,
        status,
        content: vec![sdk::ToolResultContent::Text(text)],
    }))
}

/// Pure per-part mapping of a multi-part user message onto SDK `UserContent`.
/// Image bytes are forwarded verbatim. Validation errors are normalized
/// before any request is sent and never include the image bytes.
fn map_user_parts(parts: &[UserPart]) -> Result<Vec<sdk::UserContent>, ModelError> {
    let mut content = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            UserPart::Text(text) => content.push(sdk::UserContent::Text(text.clone())),
            UserPart::Image { media_type, bytes } => {
                let media_type =
                    sdk::ImageMediaType::new(media_type.as_str()).map_err(|error| {
                        ModelError::new(format!(
                            "model call configuration invalid: user image rejected: {error}"
                        ))
                    })?;
                let image =
                    sdk::ImageInput::from_bytes(media_type, bytes::Bytes::from(bytes.clone()))
                        .map_err(|error| {
                            ModelError::new(format!(
                                "model call configuration invalid: user image rejected: {error}"
                            ))
                        })?;
                content.push(sdk::UserContent::Image(image));
            }
        }
    }
    Ok(content)
}

fn history_error(field: &str) -> ModelError {
    ModelError::new(format!("model call history invalid: {field}"))
}
