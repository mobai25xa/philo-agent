//! Bounded latest-wins store for mergeable driver and outbound state.
//!
//! Writes overwrite in place and never allocate a new queue node. The
//! coordinator pulls these slots on its own turn; they are not a second
//! reliable FIFO.

use crate::runtime_event::{is_mergeable, merge_events};
use crate::{
    AgentEvent, MaintenanceId, ModelCallId, OperationId, OperationPhase, RuntimeEvent, TokenUsage,
    ToolCallId,
};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use tokio::sync::Notify;

const TOOL_PROGRESS_SLOTS_MAX: usize = 32;

/// One slot each for sealed text, reasoning, and usage. Reliable facts
/// merge into these slots instead of appending an unbounded Vec.
pub(crate) const SEALED_MODEL_STREAM_CAP: usize = 3;
/// Parked sealed generations from earlier model calls in the same turn.
/// Keeps the per-generation cap of three kinds without letting a later
/// call overwrite an undrained earlier call.
const SEALED_PENDING_GEN_MAX: usize = 8;

/// Coalesced driver-side transients for one active operation.
pub(crate) struct TransientDriverState {
    inner: Mutex<DriverSlots>,
    notify: Notify,
}

#[derive(Default)]
struct ModelStreamSlots {
    text: Option<String>,
    reasoning: Option<(ModelCallId, String)>,
    usage: Option<(ModelCallId, TokenUsage)>,
}

impl ModelStreamSlots {
    fn is_empty(&self) -> bool {
        self.text.is_none() && self.reasoning.is_none() && self.usage.is_none()
    }

    fn len(&self) -> usize {
        usize::from(self.text.is_some())
            + usize::from(self.reasoning.is_some())
            + usize::from(self.usage.is_some())
    }
}

#[derive(Default)]
struct DriverSlots {
    text: Option<String>,
    reasoning: Option<(ModelCallId, String)>,
    usage: Option<(ModelCallId, TokenUsage)>,
    tool_progress: HashMap<ToolCallId, AgentEvent>,
    phase: Option<OperationPhase>,
    sealed: ModelStreamSlots,
    sealed_pending: VecDeque<ModelStreamSlots>,
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
            AgentEvent::TextDelta { delta } => merge_text(&mut slots.text, delta),
            AgentEvent::ReasoningDelta {
                model_call_id,
                text,
            } => merge_reasoning(&mut slots.reasoning, model_call_id, text),
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
        self.wake();
    }

    pub(crate) fn publish_phase(&self, phase: OperationPhase) {
        lock(&self.inner).phase = Some(phase);
        self.wake();
    }

    pub(crate) fn has_data(&self) -> bool {
        let slots = lock(&self.inner);
        slots.text.is_some()
            || slots.reasoning.is_some()
            || slots.usage.is_some()
            || !slots.tool_progress.is_empty()
            || slots.phase.is_some()
            || !slots.sealed.is_empty()
            || !slots.sealed_pending.is_empty()
    }

    pub(crate) fn sealed_len(&self) -> usize {
        let slots = lock(&self.inner);
        slots.sealed.len()
            + slots
                .sealed_pending
                .iter()
                .map(ModelStreamSlots::len)
                .sum::<usize>()
    }

    pub(crate) fn sealed_cap(&self) -> usize {
        SEALED_MODEL_STREAM_CAP
    }

    /// Freezes the open model-stream slots so a later call cannot overwrite
    /// them. Called before each reliable driver fact is sent.
    pub(crate) fn seal_model_stream(&self) {
        let mut slots = lock(&self.inner);
        let open = take_open_model_stream(&mut slots);
        if open.is_empty() {
            return;
        }
        if slots.sealed.is_empty() || can_merge_stream_slots(&slots.sealed, &open) {
            merge_stream_slots(&mut slots.sealed, open);
        } else {
            if slots.sealed_pending.len() >= SEALED_PENDING_GEN_MAX {
                slots.sealed_pending.pop_front();
            }
            let previous = std::mem::take(&mut slots.sealed);
            slots.sealed_pending.push_back(previous);
            slots.sealed = open;
        }
        drop(slots);
        self.wake();
    }

    fn wake(&self) {
        // `notify_waiters` wakes current waiters; `notify_one` stores a
        // permit so a waiter that has not subscribed yet still observes
        // this publish (lost-wakeup closed).
        self.notify.notify_waiters();
        self.notify.notify_one();
    }

    pub(crate) async fn wait(&self) {
        loop {
            // Subscribe before the has_data check so a notify between the
            // two cannot be lost. Same latch pattern as cancel waits.
            let notified = self.notify.notified();
            if self.has_data() {
                return;
            }
            notified.await;
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
    let mut events = Vec::new();
    for generation in slots.sealed_pending.drain(..) {
        events.extend(model_stream_events(generation));
    }
    events.extend(model_stream_events(std::mem::take(&mut slots.sealed)));
    events.extend(model_stream_events(take_open_model_stream(slots)));
    events
}

fn take_open_model_stream(slots: &mut DriverSlots) -> ModelStreamSlots {
    ModelStreamSlots {
        reasoning: slots.reasoning.take(),
        text: slots.text.take(),
        usage: slots.usage.take(),
    }
}

fn merge_text(slot: &mut Option<String>, delta: String) {
    let text = slot.get_or_insert_with(String::new);
    text.push_str(&delta);
    if text.len() > crate::bounds::DELTA_MERGE_CHUNK_MAX {
        text.truncate(crate::bounds::DELTA_MERGE_CHUNK_MAX);
    }
}

fn merge_reasoning(
    slot: &mut Option<(ModelCallId, String)>,
    model_call_id: ModelCallId,
    text: String,
) {
    match slot {
        Some((id, held)) if *id == model_call_id => {
            held.push_str(&text);
            if held.len() > crate::bounds::DELTA_MERGE_CHUNK_MAX {
                held.truncate(crate::bounds::DELTA_MERGE_CHUNK_MAX);
            }
        }
        _ => *slot = Some((model_call_id, text)),
    }
}

fn stream_call_id(slots: &ModelStreamSlots) -> Option<&ModelCallId> {
    slots
        .reasoning
        .as_ref()
        .map(|(id, _)| id)
        .or_else(|| slots.usage.as_ref().map(|(id, _)| id))
}

fn can_merge_stream_slots(held: &ModelStreamSlots, incoming: &ModelStreamSlots) -> bool {
    match (stream_call_id(held), stream_call_id(incoming)) {
        (Some(left), Some(right)) => left == right,
        _ => true,
    }
}

fn merge_stream_slots(target: &mut ModelStreamSlots, incoming: ModelStreamSlots) {
    if let Some(delta) = incoming.text {
        merge_text(&mut target.text, delta);
    }
    if let Some((model_call_id, text)) = incoming.reasoning {
        merge_reasoning(&mut target.reasoning, model_call_id, text);
    }
    if let Some(usage) = incoming.usage {
        target.usage = Some(usage);
    }
}

fn model_stream_events(held: ModelStreamSlots) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    if let Some((model_call_id, text)) = held.reasoning {
        events.push(AgentEvent::ReasoningDelta {
            model_call_id,
            text,
        });
    }
    if let Some(delta) = held.text {
        events.push(AgentEvent::TextDelta { delta });
    }
    if let Some((model_call_id, usage)) = held.usage {
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum TransientKey {
    Text {
        operation_id: OperationId,
        model_call_id: Option<ModelCallId>,
    },
    Reasoning {
        operation_id: OperationId,
        model_call_id: ModelCallId,
    },
    Usage {
        operation_id: OperationId,
        model_call_id: ModelCallId,
    },
    ToolProgress {
        operation_id: OperationId,
    },
    Availability,
    Maintenance {
        id: MaintenanceId,
    },
}

fn transient_key(
    event: &RuntimeEvent,
    operation_id: Option<&OperationId>,
    model_call_id: Option<&ModelCallId>,
) -> Option<TransientKey> {
    match event {
        RuntimeEvent::Agent(AgentEvent::TextDelta { .. }) => Some(TransientKey::Text {
            operation_id: operation_id.cloned()?,
            model_call_id: model_call_id.cloned(),
        }),
        RuntimeEvent::Agent(AgentEvent::ReasoningDelta { model_call_id, .. }) => {
            Some(TransientKey::Reasoning {
                operation_id: operation_id.cloned()?,
                model_call_id: model_call_id.clone(),
            })
        }
        RuntimeEvent::Agent(AgentEvent::ModelUsageUpdated { model_call_id, .. }) => {
            Some(TransientKey::Usage {
                operation_id: operation_id.cloned()?,
                model_call_id: model_call_id.clone(),
            })
        }
        RuntimeEvent::Agent(AgentEvent::ToolExecutionProgress { .. }) => {
            Some(TransientKey::ToolProgress {
                operation_id: operation_id.cloned()?,
            })
        }
        RuntimeEvent::AvailabilityChanged { .. } => Some(TransientKey::Availability),
        RuntimeEvent::MaintenanceProgress { id, .. } => {
            Some(TransientKey::Maintenance { id: id.clone() })
        }
        _ => None,
    }
}

/// Keyed latest-wins store for service-facing transients. Never spills into
/// reliable staging.
pub(crate) struct TransientCoalescer {
    cap: usize,
    slots: HashMap<TransientKey, RuntimeEvent>,
    order: VecDeque<TransientKey>,
}

impl TransientCoalescer {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            cap,
            slots: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub(crate) fn cap(&self) -> usize {
        self.cap
    }

    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub(crate) fn publish(
        &mut self,
        event: RuntimeEvent,
        operation_id: Option<&OperationId>,
        model_call_id: Option<&ModelCallId>,
    ) {
        debug_assert!(is_mergeable(&event));
        let Some(key) = transient_key(&event, operation_id, model_call_id) else {
            return;
        };
        if let Some(held) = self.slots.get_mut(&key) {
            *held = merge_events(held.clone(), event.clone()).unwrap_or(event);
            return;
        }
        if self.slots.len() >= self.cap {
            if let Some(oldest) = self.order.pop_front() {
                self.slots.remove(&oldest);
            }
        }
        self.slots.insert(key.clone(), event);
        self.order.push_back(key);
    }

    pub(crate) fn take_one(&mut self) -> Option<RuntimeEvent> {
        let key = self.order.pop_front()?;
        self.slots.remove(&key)
    }

    pub(crate) fn clear(&mut self) {
        self.slots.clear();
        self.order.clear();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentAvailability, TokenUsage};

    fn text(delta: &str) -> RuntimeEvent {
        RuntimeEvent::Agent(AgentEvent::TextDelta {
            delta: delta.to_owned(),
        })
    }

    fn reasoning(id: &str, text: &str) -> RuntimeEvent {
        RuntimeEvent::Agent(AgentEvent::ReasoningDelta {
            model_call_id: ModelCallId::new(id),
            text: text.to_owned(),
        })
    }

    #[test]
    fn same_key_merges_and_never_grows() {
        let op = OperationId::new("op-1");
        let mut coalescer = TransientCoalescer::new(4);
        coalescer.publish(text("a"), Some(&op), None);
        coalescer.publish(text("b"), Some(&op), None);
        assert_eq!(coalescer.len(), 1);
        let RuntimeEvent::Agent(AgentEvent::TextDelta { delta }) = coalescer.take_one().unwrap()
        else {
            panic!("expected text");
        };
        assert_eq!(delta, "ab");
        assert!(coalescer.is_empty());
    }

    #[test]
    fn different_keys_do_not_displace_into_a_queue() {
        let op = OperationId::new("op-1");
        let mut coalescer = TransientCoalescer::new(4);
        coalescer.publish(text("a"), Some(&op), None);
        coalescer.publish(reasoning("call", "r"), Some(&op), None);
        coalescer.publish(
            RuntimeEvent::Agent(AgentEvent::ModelUsageUpdated {
                model_call_id: ModelCallId::new("call"),
                usage: TokenUsage {
                    input_tokens: Some(1),
                    output_tokens: Some(2),
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    reasoning_tokens: None,
                },
            }),
            Some(&op),
            None,
        );
        assert_eq!(coalescer.len(), 3);
    }

    fn tool_progress(tool_call_id: &str) -> RuntimeEvent {
        RuntimeEvent::Agent(AgentEvent::ToolExecutionProgress {
            tool_batch_id: crate::ToolBatchId::new("batch"),
            tool_call_id: ToolCallId::new(tool_call_id),
            index: 0,
            tail: "tail".to_owned(),
        })
    }

    #[test]
    fn tool_progress_is_one_slot_per_operation() {
        let op = OperationId::new("op-1");
        let mut coalescer = TransientCoalescer::new(4);
        coalescer.publish(text("keep"), Some(&op), None);
        coalescer.publish(tool_progress("call-a"), Some(&op), None);
        coalescer.publish(tool_progress("call-b"), Some(&op), None);
        assert_eq!(coalescer.len(), 2);
        let first = coalescer.take_one().unwrap();
        assert!(matches!(
            first,
            RuntimeEvent::Agent(AgentEvent::TextDelta { .. })
        ));
        let second = coalescer.take_one().unwrap();
        match second {
            RuntimeEvent::Agent(AgentEvent::ToolExecutionProgress { tool_call_id, .. }) => {
                assert_eq!(tool_call_id.as_str(), "call-b");
            }
            other => panic!("expected latest tool progress, got {other:?}"),
        }
    }

    #[test]
    fn over_cap_evicts_oldest_transient() {
        let op = OperationId::new("op-1");
        let mut coalescer = TransientCoalescer::new(1);
        coalescer.publish(text("keep-me-not"), Some(&op), None);
        coalescer.publish(
            RuntimeEvent::AvailabilityChanged {
                availability: AgentAvailability::Idle,
                queued: 0,
            },
            None,
            None,
        );
        assert_eq!(coalescer.len(), 1);
        assert!(matches!(
            coalescer.take_one(),
            Some(RuntimeEvent::AvailabilityChanged { .. })
        ));
    }

    #[test]
    fn seal_merges_like_open_slots_and_never_grows_past_cap() {
        let state = TransientDriverState::new();
        for index in 0..32 {
            state.publish_agent(AgentEvent::TextDelta {
                delta: format!("t{index}"),
            });
            state.publish_agent(AgentEvent::ReasoningDelta {
                model_call_id: ModelCallId::new("call"),
                text: format!("r{index}"),
            });
            state.publish_agent(AgentEvent::ModelUsageUpdated {
                model_call_id: ModelCallId::new("call"),
                usage: TokenUsage {
                    input_tokens: Some(index as u64),
                    output_tokens: Some(1),
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    reasoning_tokens: None,
                },
            });
            state.seal_model_stream();
            assert!(
                state.sealed_len() <= SEALED_MODEL_STREAM_CAP,
                "sealed stream grew past cap after {} seals",
                index + 1
            );
        }
        let (_, events) = state.drain();
        assert!(events.len() <= SEALED_MODEL_STREAM_CAP);
        let text = events.iter().find_map(|event| match event {
            AgentEvent::TextDelta { delta } => Some(delta.as_str()),
            _ => None,
        });
        assert!(text.is_some_and(|delta| delta.contains("t0") && delta.contains("t31")));
    }

    #[test]
    fn later_model_call_does_not_overwrite_undrained_sealed_reasoning() {
        let state = TransientDriverState::new();
        state.publish_agent(AgentEvent::ReasoningDelta {
            model_call_id: ModelCallId::new("call-1"),
            text: "planning the read".into(),
        });
        state.seal_model_stream();
        state.publish_agent(AgentEvent::ReasoningDelta {
            model_call_id: ModelCallId::new("call-2"),
            text: "summarizing".into(),
        });
        state.seal_model_stream();
        let (_, events) = state.drain();
        let reasoning: Vec<(&str, &str)> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ReasoningDelta {
                    model_call_id,
                    text,
                } => Some((model_call_id.as_str(), text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasoning,
            vec![("call-1", "planning the read"), ("call-2", "summarizing")]
        );
    }
}
