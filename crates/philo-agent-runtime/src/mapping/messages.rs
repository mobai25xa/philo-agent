//! Projections of durable context and in-turn messages into model messages.

use super::parts::{runtime_parts_from_kernel, runtime_parts_from_session};
use crate::{ModelMessage, ModelToolCall, ModelToolResultOutcome, ToolCallId, TurnSnapshot};
use philo_agent_kernel as kernel;
use philo_session as session;

/// Projects durable context messages into model messages, synthesizing an
/// `Interrupted` placeholder for every dangling tool call (M11).
///
/// Dangling calls in the projected stream can only belong to terminated
/// turns — historical failure transactions never complete their batch, and
/// open turns are sealed before this projection is built. The placeholders
/// are a pure mapping-layer fact: nothing is written to the session, and
/// they share the adapter rendering of durable `Interrupted` results.
pub(crate) fn context_messages(context: &session::SessionContextView) -> Vec<ModelMessage> {
    let mut output = Vec::with_capacity(context.messages().len());
    // Source order guarantees a batch's results directly follow it, so the
    // pending list always describes the most recent batch's missing suffix.
    let mut pending: Vec<ToolCallId> = Vec::new();
    for message in context.messages() {
        match message {
            session::ContextMessage::ToolResult {
                tool_call_id,
                outcome,
            } => {
                if !pending.is_empty() {
                    pending.remove(0);
                }
                output.push(ModelMessage::ToolResult {
                    tool_call_id: ToolCallId::new(tool_call_id.as_str()),
                    outcome: match outcome {
                        session::ToolResultOutcome::Success { content } => {
                            ModelToolResultOutcome::Success {
                                content: content.clone(),
                            }
                        }
                        session::ToolResultOutcome::Error { code, message } => {
                            ModelToolResultOutcome::Error {
                                code: code.clone(),
                                message: message.clone(),
                            }
                        }
                        session::ToolResultOutcome::Cancelled => ModelToolResultOutcome::Cancelled,
                        session::ToolResultOutcome::Interrupted => {
                            ModelToolResultOutcome::Interrupted
                        }
                    },
                });
            }
            other => {
                flush_interrupted_placeholders(&mut output, &mut pending);
                match other {
                    session::ContextMessage::Summary { text } => {
                        output.push(ModelMessage::Summary { text: text.clone() });
                    }
                    session::ContextMessage::User { parts } => output.push(ModelMessage::User {
                        parts: runtime_parts_from_session(parts),
                    }),
                    session::ContextMessage::Assistant { content } => {
                        output.push(ModelMessage::Assistant {
                            content: content.clone(),
                        });
                    }
                    session::ContextMessage::AssistantToolCalls { calls, .. } => {
                        pending = calls
                            .iter()
                            .map(|call| ToolCallId::new(call.id().as_str()))
                            .collect();
                        output.push(ModelMessage::AssistantToolCalls {
                            calls: calls
                                .iter()
                                .map(|call| ModelToolCall {
                                    tool_call_id: ToolCallId::new(call.id().as_str()),
                                    name: call.name().to_owned(),
                                    arguments: call.arguments().to_owned(),
                                })
                                .collect(),
                        });
                    }
                    session::ContextMessage::ToolResult { .. } => unreachable!("matched above"),
                }
            }
        }
    }
    flush_interrupted_placeholders(&mut output, &mut pending);
    output
}

fn flush_interrupted_placeholders(output: &mut Vec<ModelMessage>, pending: &mut Vec<ToolCallId>) {
    for tool_call_id in pending.drain(..) {
        output.push(ModelMessage::ToolResult {
            tool_call_id,
            outcome: ModelToolResultOutcome::Interrupted,
        });
    }
}

fn turn_messages(messages: Vec<kernel::TurnMessage>) -> Vec<ModelMessage> {
    messages
        .into_iter()
        .map(|message| match message {
            kernel::TurnMessage::User(user) => ModelMessage::User {
                parts: runtime_parts_from_kernel(user.parts()),
            },
            kernel::TurnMessage::Assistant(output) => ModelMessage::Assistant {
                content: output.text().to_owned(),
            },
            kernel::TurnMessage::AssistantToolCalls { calls, .. } => {
                ModelMessage::AssistantToolCalls {
                    calls: calls
                        .into_iter()
                        .map(|call| ModelToolCall {
                            tool_call_id: ToolCallId::new(call.id().as_str()),
                            name: call.name().to_owned(),
                            arguments: call.arguments().to_owned(),
                        })
                        .collect(),
                }
            }
            kernel::TurnMessage::ToolResult(result) => ModelMessage::ToolResult {
                tool_call_id: ToolCallId::new(result.call_id().as_str()),
                outcome: match result.outcome() {
                    kernel::KernelToolResultOutcome::Success { content } => {
                        ModelToolResultOutcome::Success {
                            content: content.clone(),
                        }
                    }
                    kernel::KernelToolResultOutcome::Error { code, message } => {
                        ModelToolResultOutcome::Error {
                            code: code.clone(),
                            message: message.clone(),
                        }
                    }
                },
            },
        })
        .collect()
}

/// Assembles the full message list of one model call: system prompt, the
/// frozen context, then the in-turn messages.
pub(crate) fn build_messages(
    turn: &TurnSnapshot,
    messages: Vec<kernel::TurnMessage>,
) -> Vec<ModelMessage> {
    let mut output = vec![ModelMessage::System {
        content: turn.system_prompt.clone(),
    }];
    output.extend(turn.context_messages.clone());
    output.extend(turn_messages(messages));
    output
}
