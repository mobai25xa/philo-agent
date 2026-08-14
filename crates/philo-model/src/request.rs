use std::num::NonZeroU32;

use philo::api::stable as sdk;
use philo_agent_runtime::{
    ModelCallSnapshot, ModelError, ModelMessage, ModelToolResultOutcome, ReasoningEffort,
    ToolChoice, ToolDefinition, UserPart,
};

use crate::replay::CachedReasoning;

/// Model-visible placeholder for an empty tool success text: the SDK requires
/// every tool-result message to carry at least one non-empty text block.
pub(crate) const EMPTY_TOOL_RESULT_TEXT: &str = "(empty)";

/// Canonical, stable model-visible text replayed for a tool call that never
/// executed because its turn was cancelled.
pub(crate) const CANCELLED_TOOL_RESULT_TEXT: &str =
    "cancelled: the tool call did not execute because the turn was cancelled";

/// Canonical, stable model-visible text for a tool call whose process was
/// interrupted (M11): execution state unknown, side effects may have
/// happened. Shared by durable seal results and the runtime's synthesized
/// placeholders for dangling batches of terminated turns.
pub(crate) const INTERRUPTED_TOOL_RESULT_TEXT: &str = "interrupted: the process was interrupted \
     while this call was outstanding; whether it executed is unknown, so verify the actual state \
     before assuming";

/// Maps an immutable `ModelCallSnapshot` onto a provider-neutral SDK request.
///
/// System prompt becomes `instructions`; assistant history is synthesized
/// into blocks with contiguous zero-based indices and response-unique block
/// IDs; tool exposure follows the snapshot: non-empty tools map to
/// `ToolChoice::Auto`, empty tools map to `ToolChoice::None`.
///
/// Tool result errors always carry a `code: message` text block. The SDK
/// status is `Error` only when the resolved target natively supports an
/// error status (`native_error_status`); other protocols would reject the
/// request in capability preflight, so the error text travels as a normal
/// successful tool-result message instead.
///
/// `replayed` carries the reasoning captured by this turn's earlier calls
/// (entry k for logical call k+1). The current turn's assistant tool-call
/// messages are the trailing `replayed.len()` such messages in the snapshot;
/// each gets its call's reasoning items injected verbatim ahead of its tool
/// calls, so providers that verify reasoning state accept the replay. An
/// empty `replayed` (a turn's first call, or targets without reasoning)
/// keeps the pre-M7 request shape.
pub(crate) fn map_request(
    snapshot: &ModelCallSnapshot,
    native_error_status: bool,
    replayed: &[Vec<CachedReasoning>],
) -> Result<sdk::ModelRequest, ModelError> {
    let max_output_tokens =
        NonZeroU32::new(snapshot.generation.max_output_tokens).ok_or_else(|| {
            ModelError::new(
                "model call configuration invalid: generation.max_output_tokens must be greater than zero",
            )
        })?;
    let mut request = sdk::ModelRequest::new(max_output_tokens);
    // OpenAI reasoning models reject sampling controls such as temperature.
    // Keep the configured value for baseline requests, but omit it whenever
    // the caller explicitly selects a reasoning effort.
    request.generation.temperature = snapshot
        .generation
        .reasoning_effort
        .is_none()
        .then(|| f64::from(snapshot.generation.temperature));
    request.reasoning = map_reasoning_config(snapshot.generation.reasoning_effort);

    // The trailing `replayed.len()` assistant tool-call messages belong to
    // the current turn: batch k was produced by logical call k.
    let total_batches = snapshot
        .messages
        .iter()
        .filter(|message| matches!(message, ModelMessage::AssistantToolCalls { .. }))
        .count();
    let replay_offset = total_batches.saturating_sub(replayed.len());
    let mut batch_sequence = 0usize;

    let mut instructions = Vec::new();
    for message in &snapshot.messages {
        match message {
            ModelMessage::System { content } => instructions.push(content.as_str()),
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
                // Reasoning items replay first, then the tool calls, with
                // contiguous zero-based indices and response-unique block IDs.
                let reasoning_items: &[CachedReasoning] = if batch_sequence >= replay_offset {
                    replayed
                        .get(batch_sequence - replay_offset)
                        .map_or(&[], Vec::as_slice)
                } else {
                    &[]
                };
                batch_sequence += 1;
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
                    let name = sdk::ToolName::new(call.name.as_str())
                        .map_err(|_| history_error("tool call name"))?;
                    let tool_call =
                        match sdk::ToolCall::from_raw(id.clone(), name.clone(), &call.arguments) {
                            Ok(tool_call) => tool_call,
                            // Replay degradation: raw arguments that are not a
                            // valid JSON object replay as `{}`. Durable session
                            // facts are untouched; the full error text travels
                            // in the paired ToolResult::Error message.
                            Err(_) => sdk::ToolCall::from_raw(id, name, "{}")
                                .expect("`{}` is a valid JSON object"),
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
                request
                    .messages
                    .push(sdk::Message::Assistant { content: items });
            }
            ModelMessage::ToolResult {
                tool_call_id,
                outcome,
            } => {
                let call_id = sdk::ToolCallId::new(tool_call_id.as_str())
                    .map_err(|_| history_error("tool result call id"))?;
                let (status, text) = match outcome {
                    ModelToolResultOutcome::Success { content } => (
                        sdk::ToolResultStatus::Success,
                        if content.is_empty() {
                            EMPTY_TOOL_RESULT_TEXT.to_owned()
                        } else {
                            content.clone()
                        },
                    ),
                    ModelToolResultOutcome::Error { code, message } => (
                        if native_error_status {
                            sdk::ToolResultStatus::Error
                        } else {
                            sdk::ToolResultStatus::Success
                        },
                        format!("{code}: {message}"),
                    ),
                    // Cancellation follows the M4 status-adaptation precedent:
                    // canonical text always travels; the Error status is used
                    // only where the target supports it natively.
                    ModelToolResultOutcome::Cancelled => (
                        if native_error_status {
                            sdk::ToolResultStatus::Error
                        } else {
                            sdk::ToolResultStatus::Success
                        },
                        CANCELLED_TOOL_RESULT_TEXT.to_owned(),
                    ),
                    // Interruption (M11) shares the same status adaptation;
                    // one rendering rule covers durable seal results and
                    // synthesized placeholders alike.
                    ModelToolResultOutcome::Interrupted => (
                        if native_error_status {
                            sdk::ToolResultStatus::Error
                        } else {
                            sdk::ToolResultStatus::Success
                        },
                        INTERRUPTED_TOOL_RESULT_TEXT.to_owned(),
                    ),
                };
                request
                    .messages
                    .push(sdk::Message::ToolResult(sdk::ToolResultMessage {
                        call_id,
                        status,
                        content: vec![sdk::ToolResultContent::Text(text)],
                    }));
            }
        }
    }
    let instructions = instructions.join("\n\n");
    if !instructions.is_empty() {
        request.instructions = Some(instructions);
    }

    if snapshot.tools.is_empty() {
        // Tool disabling wins: the frozen configuration has no effect on a
        // tools_allowed = false call (pre-M10 behavior unchanged).
        request.tool_choice = sdk::ToolChoice::None;
    } else {
        request.tool_choice = map_tool_choice(&snapshot.generation.tool_choice, &snapshot.tools)?;
        for tool in &snapshot.tools {
            request.tools.push(map_tool(tool)?);
        }
    }
    // Kernel serial invariant: parallel tool calls stay locked off and are
    // never exposed through configuration.
    request.parallel_tool_calls = sdk::ParallelToolCalls::Forbid;
    Ok(request)
}

/// Direct mapping of the frozen runtime tool choice onto the SDK vocabulary.
/// `Specific` is validated against the frozen tool definitions before any
/// transport call: an unknown name is a configuration error (M4 decision 6
/// precedent, established failure path).
fn map_tool_choice(
    choice: &ToolChoice,
    tools: &[ToolDefinition],
) -> Result<sdk::ToolChoice, ModelError> {
    Ok(match choice {
        ToolChoice::Auto => sdk::ToolChoice::Auto,
        ToolChoice::None => sdk::ToolChoice::None,
        ToolChoice::Required => sdk::ToolChoice::Required,
        ToolChoice::Specific { name } => {
            if !tools.iter().any(|tool| tool.name() == name) {
                return Err(ModelError::new(format!(
                    "model call configuration invalid: tool_choice requires '{name}', \
                     which is not among the frozen tool definitions"
                )));
            }
            sdk::ToolChoice::Specific(sdk::ToolName::new(name.as_str()).map_err(|_| {
                ModelError::new(format!(
                    "model call configuration invalid: tool_choice name '{name}' is not a \
                     valid tool name"
                ))
            })?)
        }
    })
}

/// Pure per-part mapping of a multi-part user message onto SDK `UserContent`.
/// Image bytes are forwarded verbatim (the SDK neither downloads nor
/// transcodes). SDK image validation failures (illegal media type, empty
/// bytes) normalize into configuration `ModelError`s before any request is
/// sent; the diagnostic carries field and rule text only, never bytes.
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

/// Maps the frozen runtime reasoning setting onto the SDK request config.
/// `None` keeps the SDK default (reasoning untouched, pre-M7 request shape);
/// an unsupported effort for the resolved target is rejected by SDK
/// capability preflight and normalized into a configuration `ModelError`.
fn map_reasoning_config(effort: Option<ReasoningEffort>) -> sdk::ReasoningConfig {
    match effort {
        None => sdk::ReasoningConfig::default(),
        Some(effort) => sdk::ReasoningConfig {
            mode: sdk::ReasoningMode::Effort(match effort {
                ReasoningEffort::Minimal => sdk::ReasoningEffort::Minimal,
                ReasoningEffort::Low => sdk::ReasoningEffort::Low,
                ReasoningEffort::Medium => sdk::ReasoningEffort::Medium,
                ReasoningEffort::High => sdk::ReasoningEffort::High,
                ReasoningEffort::VeryHigh => sdk::ReasoningEffort::VeryHigh,
                ReasoningEffort::Maximum => sdk::ReasoningEffort::Maximum,
            }),
            report: sdk::ReasoningReport::None,
        },
    }
}

fn map_tool(tool: &ToolDefinition) -> Result<sdk::ToolDefinition, ModelError> {
    let name = sdk::ToolName::new(tool.name())
        .map_err(|_| ModelError::new("frozen tool definition has an invalid name"))?;
    let schema: serde_json::Value = serde_json::from_str(tool.parameters().as_str())
        .map_err(|_| ModelError::new("frozen tool definition has an invalid parameter schema"))?;
    let parameters = sdk::JsonSchema::new(schema)
        .map_err(|_| ModelError::new("frozen tool definition schema root must be an object"))?;
    Ok(sdk::ToolDefinition {
        name,
        description: (!tool.description().is_empty()).then(|| tool.description().to_owned()),
        parameters,
        strictness: sdk::ToolStrictness::BestEffort,
    })
}

fn history_error(field: &str) -> ModelError {
    ModelError::new(format!("model call history invalid: {field}"))
}
