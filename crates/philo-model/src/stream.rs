use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use philo::api::stable as sdk;
use philo_agent_runtime::{ModelError, ModelEvent, ModelEventStream, RuntimeFuture, TokenUsage};

use crate::error::model_error;
use crate::replay::{CachedReasoning, ReplayChannel};

/// Normalizes the SDK event stream into the runtime `ModelEvent` vocabulary.
///
/// Tool-call blocks are registered on `ToolCallStarted` and mapped to a
/// stable batch index in first-appearance source order; name/argument deltas
/// keep that order and carry the stable call id exactly once. Visible
/// reasoning deltas surface as transient `ReasoningDelta` events while opaque
/// (redacted) reasoning produces no event at all; every finished reasoning
/// block is captured verbatim into the turn's replay side channel. Usage
/// updates map onto the runtime `TokenUsage`. Structured-output events and
/// block start/finish boundaries are dropped. `ResponseFinished` maps to the
/// unique `Completed` and commits the captured reasoning to the channel; a
/// mid-stream SDK error terminates the stream as a `ModelError` without a
/// later `Completed`.
pub(crate) struct NormalizedStream {
    call: sdk::ModelCall,
    tools: HashMap<sdk::BlockId, ToolEntry>,
    next_tool_index: usize,
    reasoning: HashMap<sdk::BlockId, ReasoningEntry>,
    captured: Vec<CachedReasoning>,
    channel: Arc<ReplayChannel>,
    turn_key: String,
    call_index: u32,
    done: bool,
}

struct ToolEntry {
    index: usize,
    call_id: String,
    announced: bool,
}

struct ReasoningEntry {
    kind: sdk::ReasoningKind,
    replay_requirement: sdk::ReplayRequirement,
    text: String,
}

impl NormalizedStream {
    pub(crate) fn new(
        call: sdk::ModelCall,
        channel: Arc<ReplayChannel>,
        turn_key: String,
        call_index: u32,
    ) -> Self {
        Self {
            call,
            tools: HashMap::new(),
            next_tool_index: 0,
            reasoning: HashMap::new(),
            captured: Vec::new(),
            channel,
            turn_key,
            call_index,
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
                        return Some(Ok(ModelEvent::ResponseStarted {
                            response_model: metadata.response_model().map(ToOwned::to_owned),
                            response_id: metadata.response_id().map(ToOwned::to_owned),
                        }));
                    }
                    sdk::ModelEvent::TextDelta { delta, .. } => {
                        return Some(Ok(ModelEvent::TextDelta(delta)));
                    }
                    sdk::ModelEvent::ReasoningStarted {
                        block_id,
                        kind,
                        replay_requirement,
                        ..
                    } => {
                        self.reasoning.insert(
                            block_id,
                            ReasoningEntry {
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
                                self.captured.push(CachedReasoning {
                                    kind: entry.kind,
                                    text,
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
                        block_id, call_id, ..
                    } => {
                        let index = self.next_tool_index;
                        self.next_tool_index += 1;
                        self.tools.insert(
                            block_id,
                            ToolEntry {
                                index,
                                call_id: call_id.as_str().to_owned(),
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
                    sdk::ModelEvent::ToolCallFinished { block_id, .. } => {
                        // Guarantee the stable id was surfaced at least once so
                        // a registered call can never silently vanish.
                        if let Some(entry) = self.tools.get_mut(&block_id)
                            && !entry.announced
                        {
                            entry.announced = true;
                            return Some(Ok(ModelEvent::ToolCallDelta {
                                index: entry.index,
                                id: Some(entry.call_id.clone()),
                                name: None,
                                arguments: String::new(),
                            }));
                        }
                    }
                    sdk::ModelEvent::ResponseFinished { .. } => {
                        self.done = true;
                        // Commit this call's reasoning to the turn's replay
                        // side channel exactly once, at normal completion.
                        self.channel.record(
                            &self.turn_key,
                            self.call_index,
                            std::mem::take(&mut self.captured),
                        );
                        return Some(Ok(ModelEvent::Completed));
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
