//! State shared between an operation's handle, its engine, and the scheduler.

use super::scheduler::Scheduler;
use crate::{
    AgentEvent, OperationId, OperationOutcome, OperationPhase, OperationStatus, ToolCallId, TurnId,
};
use philo_session::CancelReason;
use philo_tools::ToolCancel;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(super) struct SharedInner {
    pub(super) phase: OperationPhase,
    /// Live event queue: publication enqueues immediately (M10 real-time
    /// obligation), consumption pops through the handle.
    pub(super) events: VecDeque<AgentEvent>,
    pub(super) outcome: Option<OperationOutcome>,
    /// The consumer waiting on events or the outcome.
    pub(super) waker: Option<std::task::Waker>,
}

/// State shared between an [`super::OperationHandle`], its engine, and the
/// scheduler.
pub(crate) struct OperationShared {
    pub(super) operation_id: OperationId,
    pub(super) turn_id: TurnId,
    pub(super) scheduler: Arc<Scheduler>,
    cancel_requested: AtomicBool,
    /// Eager tool-port token; requested in the same moment as the
    /// operation-level cancel flag.
    tool_cancel: ToolCancel,
    /// First accepted cancellation reason; the winner of a user/timeout
    /// race decides how the terminal facts are recorded.
    cancel_reason: Mutex<Option<CancelReason>>,
    /// Automatic-cancellation deadline, armed when driving actually starts
    /// (dequeue time); `Queued` waiting never counts.
    deadline: Mutex<Option<Instant>>,
    pub(super) inner: Mutex<SharedInner>,
}

impl OperationShared {
    pub(crate) fn new(
        operation_id: OperationId,
        turn_id: TurnId,
        scheduler: Arc<Scheduler>,
        phase: OperationPhase,
    ) -> Self {
        Self {
            operation_id,
            turn_id,
            scheduler,
            cancel_requested: AtomicBool::new(false),
            tool_cancel: ToolCancel::new(),
            cancel_reason: Mutex::new(None),
            deadline: Mutex::new(None),
            inner: Mutex::new(SharedInner {
                phase,
                events: VecDeque::new(),
                outcome: None,
                waker: None,
            }),
        }
    }

    pub(crate) fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }
    pub(crate) fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    /// Publishes one event: enqueued and consumable immediately.
    pub(crate) fn publish(&self, event: AgentEvent) {
        let waker = {
            let mut inner = self.inner.lock().expect("operation mutex");
            inner.events.push_back(event);
            inner.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Publishes a tool-progress snapshot, replacing any unconsumed
    /// progress for the same `tool_call_id` instead of growing the queue.
    pub(crate) fn publish_tool_progress(&self, event: AgentEvent) {
        let Some(call_id) = progress_call_id(&event) else {
            self.publish(event);
            return;
        };
        let waker = {
            let mut inner = self.inner.lock().expect("operation mutex");
            if let Some(existing) = inner.events.iter_mut().rev().find(|queued| {
                matches!(
                    queued,
                    AgentEvent::ToolExecutionProgress { tool_call_id, .. }
                        if *tool_call_id == call_id
                )
            }) {
                *existing = event;
            } else {
                inner.events.push_back(event);
            }
            inner.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub(crate) fn set_phase(&self, phase: OperationPhase) {
        self.inner.lock().expect("operation mutex").phase = phase;
    }

    pub(crate) fn phase(&self) -> OperationPhase {
        self.inner.lock().expect("operation mutex").phase.clone()
    }

    /// Accepts a cancellation request with its reason; the first acceptance
    /// wins a user/timeout race and later requests change nothing.
    pub(crate) fn request_cancel(&self, reason: CancelReason) {
        let mut slot = self.cancel_reason.lock().expect("cancel reason mutex");
        if slot.is_none() {
            *slot = Some(reason);
            self.cancel_requested.store(true, Ordering::SeqCst);
            self.tool_cancel.request();
        }
    }

    /// Token cloned into every in-flight `invoke`.
    pub(crate) fn tool_cancel(&self) -> ToolCancel {
        self.tool_cancel.clone()
    }

    /// The reason of the accepted cancellation; meaningful only after
    /// [`OperationShared::is_cancel_requested`] returned true.
    pub(crate) fn cancel_reason(&self) -> CancelReason {
        self.cancel_reason
            .lock()
            .expect("cancel reason mutex")
            .unwrap_or(CancelReason::User)
    }

    /// Arms the automatic-cancellation deadline at drive start.
    pub(crate) fn arm_deadline(&self, timeout: Option<Duration>) {
        *self.deadline.lock().expect("deadline mutex") =
            timeout.map(|timeout| Instant::now() + timeout);
    }

    /// Lazily checked at the M6 injection points (and between stream
    /// events): an expired deadline requests cancellation with reason
    /// `Timeout`, losing gracefully if a user cancel arrived first.
    pub(crate) fn is_cancel_requested(&self) -> bool {
        if !self.cancel_requested.load(Ordering::SeqCst)
            && self
                .deadline
                .lock()
                .expect("deadline mutex")
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.request_cancel(CancelReason::Timeout);
        }
        self.cancel_requested.load(Ordering::SeqCst)
    }

    pub(super) fn settle(&self, outcome: OperationOutcome) {
        let status = match &outcome {
            OperationOutcome::Succeeded { .. } => OperationStatus::Succeeded,
            OperationOutcome::Failed { .. } => OperationStatus::Failed,
            OperationOutcome::Cancelled => OperationStatus::Cancelled,
        };
        let waker = {
            let mut inner = self.inner.lock().expect("operation mutex");
            inner.phase = OperationPhase::Settled(status);
            inner.outcome = Some(outcome);
            inner.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub(super) fn settled_outcome(&self) -> Option<OperationOutcome> {
        self.inner.lock().expect("operation mutex").outcome.clone()
    }

    pub(super) fn pop_event(&self) -> Option<AgentEvent> {
        self.inner
            .lock()
            .expect("operation mutex")
            .events
            .pop_front()
    }

    pub(super) fn register_waker(&self, cx: &std::task::Context<'_>) {
        self.inner.lock().expect("operation mutex").waker = Some(cx.waker().clone());
    }
}

fn progress_call_id(event: &AgentEvent) -> Option<ToolCallId> {
    match event {
        AgentEvent::ToolExecutionProgress { tool_call_id, .. } => Some(tool_call_id.clone()),
        _ => None,
    }
}
