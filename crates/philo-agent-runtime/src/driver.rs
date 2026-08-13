use crate::{
    AgentFailure, AgentFailureKind, ModelMessage, ModelToolCall, ModelToolResultOutcome,
    ToolCallDelta, ToolCallId, UserPart,
};
use philo_agent_kernel as kernel;
use philo_session as session;
use std::collections::{HashMap, HashSet};

// Explicit per-field mapping chain for multi-part user payloads: each layer
// owns its type, and image bytes pass through byte-for-byte.

pub(crate) fn kernel_user_parts(parts: &[UserPart]) -> Vec<kernel::UserPart> {
    parts
        .iter()
        .map(|part| match part {
            UserPart::Text(text) => kernel::UserPart::Text(text.clone()),
            UserPart::Image { media_type, bytes } => kernel::UserPart::Image {
                media_type: media_type.clone(),
                bytes: bytes.clone(),
            },
        })
        .collect()
}

pub(crate) fn session_user_parts(parts: &[kernel::UserPart]) -> Vec<session::SessionUserPart> {
    parts
        .iter()
        .map(|part| match part {
            kernel::UserPart::Text(text) => session::SessionUserPart::Text(text.clone()),
            kernel::UserPart::Image { media_type, bytes } => session::SessionUserPart::Image {
                media_type: media_type.clone(),
                bytes: bytes.clone(),
            },
        })
        .collect()
}

fn runtime_parts_from_session(parts: &[session::SessionUserPart]) -> Vec<UserPart> {
    parts
        .iter()
        .map(|part| match part {
            session::SessionUserPart::Text(text) => UserPart::Text(text.clone()),
            session::SessionUserPart::Image { media_type, bytes } => UserPart::Image {
                media_type: media_type.clone(),
                bytes: bytes.clone(),
            },
        })
        .collect()
}

fn runtime_parts_from_kernel(parts: &[kernel::UserPart]) -> Vec<UserPart> {
    parts
        .iter()
        .map(|part| match part {
            kernel::UserPart::Text(text) => UserPart::Text(text.clone()),
            kernel::UserPart::Image { media_type, bytes } => UserPart::Image {
                media_type: media_type.clone(),
                bytes: bytes.clone(),
            },
        })
        .collect()
}

#[derive(Default)]
pub(crate) struct OutputAssembler {
    text: String,
    calls: HashMap<usize, CallParts>,
    order: Vec<usize>,
}
#[derive(Default)]
struct CallParts {
    id: String,
    name: String,
    arguments: String,
}
impl OutputAssembler {
    pub fn text(&mut self, delta: &str) {
        self.text.push_str(delta);
    }
    pub fn tool(&mut self, delta: ToolCallDelta) {
        if !self.calls.contains_key(&delta.index) {
            self.order.push(delta.index);
        }
        let parts = self.calls.entry(delta.index).or_default();
        if let Some(id) = delta.id {
            parts.id.push_str(&id);
        }
        if let Some(name) = delta.name {
            parts.name.push_str(&name);
        }
        parts.arguments.push_str(&delta.arguments);
    }
    pub fn finish(self) -> Result<(String, Vec<kernel::KernelToolCall>), AgentFailure> {
        if !self.calls.is_empty() && !self.text.is_empty() {
            return Err(invalid("model mixed text and tool calls"));
        }
        let mut ids = HashSet::new();
        let mut calls = Vec::new();
        for index in self.order {
            let parts = self.calls.get(&index).expect("recorded call index exists");
            if parts.id.is_empty() || parts.name.trim().is_empty() || !ids.insert(parts.id.clone())
            {
                return Err(invalid("model produced incomplete or duplicate tool calls"));
            }
            calls.push(kernel::KernelToolCall::new(
                kernel::ToolCallId::new(&parts.id),
                &parts.name,
                &parts.arguments,
            ));
        }
        Ok((self.text, calls))
    }
}

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

pub(crate) fn turn_messages(messages: Vec<kernel::TurnMessage>) -> Vec<ModelMessage> {
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

pub(crate) fn invalid(message: impl Into<String>) -> AgentFailure {
    AgentFailure::new(AgentFailureKind::InvalidModelOutput, message)
}
pub(crate) fn driver(message: impl Into<String>) -> AgentFailure {
    AgentFailure::new(AgentFailureKind::RuntimeDriver, message)
}
