//! Bounded latest-wins store for mergeable driver and outbound state.
//!
//! Writes overwrite in place and never allocate a new queue node. The
//! coordinator pulls these slots on its own turn; they are not a second
//! reliable FIFO.

use crate::runtime_event::{is_mergeable, merge_events};
use crate::{AgentEvent, ModelCallId, OperationPhase, RuntimeEvent, TokenUsage, ToolCallId};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::Notify;

const TOOL_PROGRESS_SLOTS_MAX: usize = 32;

/// Coalesced driver-side transients for one active operation.
pub(crate) struct TransientDriverState {
    inner: Mutex<DriverSlots>,
    notify: Notify,
}

#[derive(Default)]
struct DriverSlots {
    text: Option<String>,
    reasoning: Option<(ModelCallId, String)>,
    usage: Option<(ModelCallId, TokenUsage)>,
    tool_progress: HashMap<ToolCallId, AgentEvent>,
    phase: Option<OperationPhase>,
    sealed_model_stream: Vec<AgentEvent>,
}

impl TransientDriverState {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(DriverSlots::default()),
            notify: Notify::new(),
        }
    }

    pub(crate) fn publish_agent(&self, event: AgentEvent) {
        let mut slots = lock(&self.inner);
        match event {
            AgentEvent::TextDelta { delta } => {
                let text = slots.text.get_or_insert_with(String::new);
                text.push_str(&delta);
                if text.len() > crate::bounds::DELTA_MERGE_CHUNK_MAX {
                    text.truncate(crate::bounds::DELTA_MERGE_CHUNK_MAX);
                }
            }
            AgentEvent::ReasoningDelta {
                model_call_id,
                text,
            } => match &mut slots.reasoning {
                Some((id, held)) if *id == model_call_id => {
                    held.push_str(&text);
                    if held.len() > crate::bounds::DELTA_MERGE_CHUNK_MAX {
                        held.truncate(crate::bounds::DELTA_MERGE_CHUNK_MAX);
                    }
                }
                _ => slots.reasoning = Some((model_call_id, text)),
            },
            AgentEvent::ModelUsageUpdated {
                model_call_id,
                usage,
            } => slots.usage = Some((model_call_id, usage)),
            AgentEvent::ToolExecutionProgress {
                ref tool_call_id, ..
            } => {
                if slots.tool_progress.len() >= TOOL_PROGRESS_SLOTS_MAX
                    && !slots.tool_progress.contains_key(tool_call_id)
                {
                    if let Some(oldest) = slots.tool_progress.keys().next().cloned() {
                        slots.tool_progress.remove(&oldest);
                    }
                }
                slots.tool_progress.insert(tool_call_id.clone(), event);
            }
            other => {
                debug_assert!(
                    false,
                    "reliable agent event published to transient store: {other:?}"
                );
            }
        }
        drop(slots);
        self.notify.notify_waiters();
    }

    pub(crate) fn publish_phase(&self, phase: OperationPhase) {
        lock(&self.inner).phase = Some(phase);
        self.notify.notify_waiters();
    }

    pub(crate) fn has_data(&self) -> bool {
        let slots = lock(&self.inner);
        slots.text.is_some()
            || slots.reasoning.is_some()
            || slots.usage.is_some()
            || !slots.tool_progress.is_empty()
            || slots.phase.is_some()
            || !slots.sealed_model_stream.is_empty()
    }

    /// Freezes the open model-stream slots so a later call cannot overwrite
    /// them. Called before each reliable driver fact is sent.
    pub(crate) fn seal_model_stream(&self) {
        let mut slots = lock(&self.inner);
        let open = take_open_model_stream(&mut slots);
        if open.is_empty() {
            return;
        }
        slots.sealed_model_stream.extend(open);
        drop(slots);
        self.notify.notify_waiters();
    }

    pub(crate) async fn wait(&self) {
        loop {
            if self.has_data() {
                return;
            }
            self.notify.notified().await;
        }
    }

    pub(crate) fn drain(&self) -> (Option<OperationPhase>, Vec<AgentEvent>) {
        let mut slots = lock(&self.inner);
        let phase = slots.phase.take();
        let mut events = drain_model_stream_from(&mut slots);
        events.extend(slots.tool_progress.drain().map(|(_, event)| event));
        (phase, events)
    }

    pub(crate) fn drain_model_stream(&self) -> Vec<AgentEvent> {
        drain_model_stream_from(&mut lock(&self.inner))
    }

    pub(crate) fn drain_tool_progress(&self) -> Vec<AgentEvent> {
        lock(&self.inner)
            .tool_progress
            .drain()
            .map(|(_, event)| event)
            .collect()
    }
}

fn drain_model_stream_from(slots: &mut DriverSlots) -> Vec<AgentEvent> {
    let mut events = std::mem::take(&mut slots.sealed_model_stream);
    events.extend(take_open_model_stream(slots));
    events
}

fn take_open_model_stream(slots: &mut DriverSlots) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    if let Some((model_call_id, text)) = slots.reasoning.take() {
        events.push(AgentEvent::ReasoningDelta {
            model_call_id,
            text,
        });
    }
    if let Some(delta) = slots.text.take() {
        events.push(AgentEvent::TextDelta { delta });
    }
    if let Some((model_call_id, usage)) = slots.usage.take() {
        events.push(AgentEvent::ModelUsageUpdated {
            model_call_id,
            usage,
        });
    }
    events
}

pub(crate) fn is_transient_agent(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::TextDelta { .. }
            | AgentEvent::ReasoningDelta { .. }
            | AgentEvent::ModelUsageUpdated { .. }
            | AgentEvent::ToolExecutionProgress { .. }
    )
}

/// Single-slot mergeable hold for the service-facing subscription.
#[derive(Default)]
pub(crate) struct TransientOutbound {
    held: Option<RuntimeEvent>,
}

impl TransientOutbound {
    /// Merges `event` into the hold. Returns a displaced previous value when
    /// the identities do not merge, so the caller can send it without dropping.
    pub(crate) fn publish(&mut self, event: RuntimeEvent) -> Option<RuntimeEvent> {
        debug_assert!(is_mergeable(&event));
        match self.held.take() {
            Some(held) => {
                if let Some(merged) = merge_events(held.clone(), event.clone()) {
                    self.held = Some(merged);
                    None
                } else {
                    self.held = Some(event);
                    Some(held)
                }
            }
            None => {
                self.held = Some(event);
                None
            }
        }
    }

    pub(crate) fn take(&mut self) -> Option<RuntimeEvent> {
        self.held.take()
    }

    pub(crate) fn restore(&mut self, event: RuntimeEvent) {
        let displaced = self.publish(event);
        debug_assert!(displaced.is_none());
        let _ = displaced;
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
