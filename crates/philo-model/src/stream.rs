use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use philo::api::stable as sdk;
use philo_agent_runtime::{
    ModelCallSnapshot, ModelError, ModelEvent, ModelEventStream, RuntimeFuture, TokenUsage,
};

use crate::error::model_error;
use crate::replay::{
    CapturedContent, CapturedItem, ReplayCoordinator, assistant_blocks_from_captured,
};

/// Normalizes the SDK event stream into the runtime `ModelEvent` vocabulary.
///
/// Tool-call blocks are registered on `ToolCallStarted` and mapped to a
/// stable batch index in first-appearance source order; name/argument deltas
/// keep that order and carry the stable call id exactly once. Visible
/// reasoning deltas surface as transient `ReasoningDelta` events while opaque
/// (redacted) reasoning produces no event at all; every finished reasoning
/// block and every other replayable response item is collected for the
/// replay sidecar. Usage updates map onto the runtime `TokenUsage`.
/// Structured-output events and block start/finish boundaries are dropped.
/// `ResponseFinished` snapshots and atomically commits the collected items
/// before yielding the unique `Completed { blocks }`; a mid-stream SDK error
/// terminates without a sidecar commit or later `Completed`.
pub(crate) struct NormalizedStream {
    call: sdk::ModelCall,
    client: sdk::PhiloClient,
    target: sdk::CallTarget,
    text: HashMap<sdk::BlockId, TextEntry>,
    tools: HashMap<sdk::BlockId, ToolEntry>,
    next_tool_index: usize,
    reasoning: HashMap<sdk::BlockId, ReasoningEntry>,
    captured: Vec<CapturedItem>,
    replay: Arc<ReplayCoordinator>,
    request: ModelCallSnapshot,
    response_id: Option<String>,
    retain_response_id: bool,
    done: bool,
}

struct ToolEntry {
    index: usize,
    response_index: u32,
    call_id: String,
    name: String,
    arguments: String,
    replay_requirement: sdk::ReplayRequirement,
    announced: bool,
}

struct TextEntry {
    index: u32,
    replay_requirement: sdk::ReplayRequirement,
    text: String,
}

struct ReasoningEntry {
    index: u32,
    kind: sdk::ReasoningKind,
    replay_requirement: sdk::ReplayRequirement,
    text: String,
}

impl NormalizedStream {
    pub(crate) fn new(
        call: sdk::ModelCall,
        client: sdk::PhiloClient,
        target: sdk::CallTarget,
        replay: Arc<ReplayCoordinator>,
        request: ModelCallSnapshot,
        retain_response_id: bool,
    ) -> Self {
        Self {
            call,
            client,
            target,
            text: HashMap::new(),
            tools: HashMap::new(),
            next_tool_index: 0,
            reasoning: HashMap::new(),
            captured: Vec::new(),
            replay,
            request,
            response_id: None,
            retain_response_id,
            done: false,
        }
    }

    fn tool_delta(
        &mut self,
        block_id: sdk::BlockId,
        name: Option<String>,
        arguments: String,
    ) -> Result<ModelEvent, ModelError> {
        let Some(entry) = self.tools.get_mut(&block_id) else {
            return Err(ModelError::new(
                "philo model stream violated protocol: tool delta before ToolCallStarted",
            ));
        };
        if let Some(delta) = name.as_deref() {
            entry.name.push_str(delta);
        }
        entry.arguments.push_str(&arguments);
        let id = if entry.announced {
            None
        } else {
            entry.announced = true;
            Some(entry.call_id.clone())
        };
        Ok(ModelEvent::ToolCallDelta {
            index: entry.index,
            id,
            name,
            arguments,
        })
    }

    fn flush_remaining_items(&mut self) {
        for (_, entry) in self.text.drain() {
            if entry.text.is_empty() {
                continue;
            }
            self.captured.push(CapturedItem {
                index: entry.index,
                content: CapturedContent::Text { text: entry.text },
                replay_requirement: entry.replay_requirement,
                replay_token: None,
            });
        }
        for (_, entry) in self.tools.drain() {
            self.captured.push(tool_captured(entry, None));
        }
    }
}

fn tool_captured(entry: ToolEntry, replay_token: Option<sdk::ReplayToken>) -> CapturedItem {
    CapturedItem {
        index: entry.response_index,
        content: CapturedContent::ToolCall {
            call_id: entry.call_id,
            name: entry.name,
            arguments: entry.arguments,
        },
        replay_requirement: entry.replay_requirement,
        replay_token,
    }
}

fn map_usage(usage: &sdk::Usage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        reasoning_tokens: usage.reasoning_tokens,
    }
}

fn visible(kind: sdk::ReasoningKind) -> bool {
    !matches!(kind, sdk::ReasoningKind::Opaque)
}

impl ModelEventStream for NormalizedStream {
    fn next<'a>(&'a mut self) -> RuntimeFuture<'a, Option<Result<ModelEvent, ModelError>>> {
        Box::pin(async move {
            if self.done {
                return None;
            }
            loop {
                let event = match self.call.next().await {
                    None => {
                        // Ended without ResponseFinished; the runtime treats a
                        // stream that never Completed as a driver failure.
                        self.done = true;
                        return None;
                    }
                    Some(Err(error)) => {
                        self.done = true;
                        return Some(Err(model_error(&error)));
                    }
                    Some(Ok(event)) => event,
                };
                match event {
                    sdk::ModelEvent::ResponseStarted { metadata } => {
                        if self.retain_response_id {
                            self.response_id = metadata.response_id().map(ToOwned::to_owned);
                        }
                        return Some(Ok(ModelEvent::ResponseStarted {
                            response_model: metadata.response_model().map(ToOwned::to_owned),
                            response_id: metadata.response_id().map(ToOwned::to_owned),
                        }));
                    }
                    sdk::ModelEvent::TextStarted {
                        block_id,
                        index,
                        replay_requirement,
                    } => {
                        self.text.insert(
                            block_id,
                            TextEntry {
                                index,
                                replay_requirement,
                                text: String::new(),
                            },
                        );
                    }
                    sdk::ModelEvent::TextDelta { block_id, delta } => {
                        if let Some(entry) = self.text.get_mut(&block_id) {
                            entry.text.push_str(&delta);
                        }
                        return Some(Ok(ModelEvent::TextDelta(delta)));
                    }
                    sdk::ModelEvent::TextFinished {
                        block_id,
                        replay_token,
                    } => {
                        if let Some(entry) = self.text.remove(&block_id) {
                            if !entry.text.is_empty() {
                                self.captured.push(CapturedItem {
                                    index: entry.index,
                                    content: CapturedContent::Text { text: entry.text },
                                    replay_requirement: entry.replay_requirement,
                                    replay_token,
                                });
                            }
                        }
                    }
                    sdk::ModelEvent::ReasoningStarted {
                        block_id,
                        index,
                        kind,
                        replay_requirement,
                        ..
                    } => {
                        self.reasoning.insert(
                            block_id,
                            ReasoningEntry {
                                index,
                                kind,
                                replay_requirement,
                                text: String::new(),
                            },
                        );
                    }
                    sdk::ModelEvent::ReasoningDelta { block_id, delta } => {
                        if let Some(entry) = self.reasoning.get_mut(&block_id) {
                            entry.text.push_str(&delta);
                            // Opaque (redacted) reasoning never surfaces as an
                            // event; it exists only for verbatim replay.
                            if visible(entry.kind) {
                                return Some(Ok(ModelEvent::ReasoningDelta { text: delta }));
                            }
                        }
                    }
                    sdk::ModelEvent::ReasoningFinished {
                        block_id,
                        replay_token,
                    } => {
                        if let Some(entry) = self.reasoning.remove(&block_id) {
                            let text = if visible(entry.kind) && !entry.text.is_empty() {
                                Some(entry.text)
                            } else {
                                None
                            };
                            // A visible block with no text has nothing valid
                            // to replay; skip it rather than build an item
                            // the SDK would reject.
                            if text.is_some() || !visible(entry.kind) {
                                self.captured.push(CapturedItem {
                                    index: entry.index,
                                    content: CapturedContent::Reasoning {
                                        kind: entry.kind,
                                        text,
                                    },
                                    replay_requirement: entry.replay_requirement,
                                    replay_token,
                                });
                            }
                        }
                    }
                    sdk::ModelEvent::UsageUpdated { usage } => {
                        return Some(Ok(ModelEvent::UsageUpdated {
                            usage: map_usage(&usage),
                        }));
                    }
                    sdk::ModelEvent::ToolCallStarted {
                        block_id,
                        index: response_index,
                        call_id,
                        replay_requirement,
                    } => {
                        let index = self.next_tool_index;
                        self.next_tool_index += 1;
                        self.tools.insert(
                            block_id,
                            ToolEntry {
                                index,
                                response_index,
                                call_id: call_id.as_str().to_owned(),
                                name: String::new(),
                                arguments: String::new(),
                                replay_requirement,
                                announced: false,
                            },
                        );
                    }
                    sdk::ModelEvent::ToolCallNameDelta { block_id, delta } => {
                        let event = self.tool_delta(block_id, Some(delta), String::new());
                        if event.is_err() {
                            self.done = true;
                        }
                        return Some(event);
                    }
                    sdk::ModelEvent::ToolCallArgumentsDelta { block_id, delta } => {
                        let event = self.tool_delta(block_id, None, delta);
                        if event.is_err() {
                            self.done = true;
                        }
                        return Some(event);
                    }
                    sdk::ModelEvent::ToolCallFinished {
                        block_id,
                        replay_token,
                    } => {
                        // Guarantee the stable id was surfaced at least once so
                        // a registered call can never silently vanish.
                        if let Some(entry) = self.tools.remove(&block_id) {
                            let fallback = (!entry.announced).then(|| ModelEvent::ToolCallDelta {
                                index: entry.index,
                                id: Some(entry.call_id.clone()),
                                name: None,
                                arguments: String::new(),
                            });
                            self.captured.push(tool_captured(entry, replay_token));
                            if let Some(fallback) = fallback {
                                return Some(Ok(fallback));
                            }
                        }
                    }
                    sdk::ModelEvent::ResponseFinished { .. } => {
                        self.done = true;
                        self.flush_remaining_items();
                        self.captured.sort_by_key(|item| item.index);
                        let blocks = assistant_blocks_from_captured(&self.captured);
                        if let Err(error) = self
                            .replay
                            .commit(
                                &self.client,
                                &self.target,
                                &self.request,
                                self.response_id.take(),
                                std::mem::take(&mut self.captured),
                            )
                            .await
                        {
                            return Some(Err(error));
                        }
                        return Some(Ok(ModelEvent::Completed { blocks }));
                    }
                    // Text/StructuredOutput block boundaries and structured
                    // output deltas have no runtime vocabulary and are
                    // dropped (registered in the capability pool).
                    _ => {}
                }
            }
        })
    }
}
