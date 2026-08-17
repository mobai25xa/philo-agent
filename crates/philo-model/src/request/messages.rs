use philo::api::stable as sdk;
use philo_agent_runtime::{
    ModelAssistantBlock, ModelError, ModelMessage, ModelToolCall, ModelToolResultOutcome, UserPart,
};

use crate::replay::{CapturedContent, CapturedItem, ReplayHistory};

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
    replayed: &ReplayHistory,
) -> Result<(), ModelError> {
    let mut system_instructions = Vec::new();
    let mut summary_instructions = Vec::new();
    let mut continuation = None;
    for (message_index, message) in messages.iter().enumerate() {
        match message {
            ModelMessage::System { content } => system_instructions.push(content.as_str()),
            ModelMessage::Summary { text } => {
                summary_instructions.push(format!("{SUMMARY_INSTRUCTION_PREFIX}{text}"))
            }
            ModelMessage::User { parts } => request.messages.push(sdk::Message::User {
                content: map_user_parts(parts)?,
            }),
            ModelMessage::Assistant { blocks } => {
                request
                    .messages
                    .push(map_assistant(blocks, replayed.items_for(message_index))?);
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
        if let Some(handle) = replayed.continuation_after(message_index) {
            continuation = Some((handle.clone(), request.messages.len()));
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
    if let Some((handle, message_start)) = continuation
        && message_start < request.messages.len()
    {
        request.continuation = Some(sdk::ResponseContinuation::continue_from(
            handle,
            message_start,
        ));
    }
    Ok(())
}

fn map_assistant(
    blocks: &[ModelAssistantBlock],
    replay_items: &[CapturedItem],
) -> Result<sdk::Message, ModelError> {
    if !replay_items.is_empty() {
        return map_replayed_assistant(replay_items, blocks);
    }
    let mut items = Vec::with_capacity(blocks.len());
    for (position, block) in blocks.iter().enumerate() {
        let content = match block {
            ModelAssistantBlock::Text { text } => {
                sdk::AssistantContent::Text { text: text.clone() }
            }
            ModelAssistantBlock::ToolCall(call) => {
                sdk::AssistantContent::ToolCall(sdk_tool_call(call)?)
            }
        };
        items.push(sdk::ResponseItem::new(
            sdk::BlockId::new(position as u64),
            u32::try_from(position).map_err(|_| history_error("assistant item index"))?,
            content,
            sdk::ReplayRequirement::None,
            None,
        ));
    }
    Ok(sdk::Message::Assistant { content: items })
}

fn map_replayed_assistant(
    replay_items: &[CapturedItem],
    blocks: &[ModelAssistantBlock],
) -> Result<sdk::Message, ModelError> {
    let mut next_text = blocks.iter().filter_map(|block| match block {
        ModelAssistantBlock::Text { text } => Some(text.as_str()),
        ModelAssistantBlock::ToolCall(_) => None,
    });
    let mut items = Vec::with_capacity(replay_items.len());
    for item in replay_items {
        let content = match &item.content {
            CapturedContent::Reasoning { kind, text } => sdk::AssistantContent::Reasoning {
                kind: *kind,
                text: text.clone(),
            },
            CapturedContent::Text { .. } => {
                let text = next_text
                    .next()
                    .ok_or_else(|| history_error("replayed text item binding"))?;
                sdk::AssistantContent::Text {
                    text: text.to_owned(),
                }
            }
            CapturedContent::ToolCall { call_id, .. } => {
                let call = blocks
                    .iter()
                    .find_map(|block| match block {
                        ModelAssistantBlock::ToolCall(call)
                            if call.tool_call_id.as_str() == call_id =>
                        {
                            Some(call)
                        }
                        _ => None,
                    })
                    .ok_or_else(|| history_error("replayed tool call binding"))?;
                sdk::AssistantContent::ToolCall(sdk_tool_call(call)?)
            }
        };
        items.push(sdk::ResponseItem::new(
            sdk::BlockId::new(u64::from(item.index)),
            item.index,
            content,
            item.replay_requirement,
            item.replay_token.clone(),
        ));
    }
    Ok(sdk::Message::Assistant { content: items })
}

fn sdk_tool_call(call: &ModelToolCall) -> Result<sdk::ToolCall, ModelError> {
    let id = sdk::ToolCallId::new(call.tool_call_id.as_str())
        .map_err(|_| history_error("tool call id"))?;
    let name =
        sdk::ToolName::new(call.name.as_str()).map_err(|_| history_error("tool call name"))?;
    match sdk::ToolCall::from_raw(id.clone(), name.clone(), &call.arguments) {
        Ok(tool_call) => Ok(tool_call),
        // Replay degradation: invalid arguments replay as `{}`. Durable
        // facts are untouched; the paired error result keeps the detail.
        Err(_) => Ok(sdk::ToolCall::from_raw(id, name, "{}").expect("`{}` is a valid JSON object")),
    }
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
