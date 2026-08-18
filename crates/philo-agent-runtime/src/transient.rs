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
}
