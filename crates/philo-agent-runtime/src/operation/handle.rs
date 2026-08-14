//! The public handle observing and driving one admitted operation.

use super::shared::OperationShared;
use crate::{
    AgentEvent, OperationId, OperationOutcome, OperationPhase, OperationStatus,
    SettlementDurability, TurnId,
};
use philo_session::CancelReason;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// The lazily driven remainder of one admitted operation.
pub(crate) type Engine = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Live handle of one admitted operation.
///
/// The handle observes phase and outcome, may request cancellation, and for
/// queued operations owns the engine that drives the work when polled.
pub struct OperationHandle {
    shared: Arc<OperationShared>,
    engine: Mutex<Option<Engine>>,
}

impl std::fmt::Debug for OperationHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperationHandle")
            .field("operation_id", &self.shared.operation_id)
            .field("phase", &self.shared.phase())
            .finish_non_exhaustive()
    }
}

impl OperationHandle {
    pub(crate) fn with_engine(shared: Arc<OperationShared>, engine: Engine) -> Self {
        Self {
            shared,
            engine: Mutex::new(Some(engine)),
        }
    }

    pub fn operation_id(&self) -> &OperationId {
        &self.shared.operation_id
    }
    pub fn turn_id(&self) -> &TurnId {
        &self.shared.turn_id
    }

    /// Returns the current phase observation.
    pub fn phase(&self) -> OperationPhase {
        self.shared.phase()
    }

    /// Requests orderly cancellation; idempotent, non-blocking, and callable
    /// at any moment while the operation runs (M10). It takes effect at the
    /// M6 injection points on barrier boundaries.
    ///
    /// Queued operations settle immediately with zero persistent trace.
    /// Once the kernel has made its final decision (`Finalizing`) or the
    /// operation settled, cancellation has no effect and publishes nothing.
    pub fn cancel(&self) {
        let scheduler = self.shared.scheduler.clone();
        let mut scheduler_inner = scheduler.inner.lock().expect("scheduler mutex");
        let mut state = self.shared.inner.lock().expect("operation mutex");
        match state.phase {
            OperationPhase::Settled(_) | OperationPhase::Finalizing => {}
            OperationPhase::Queued => {
                scheduler_inner
                    .queue
                    .retain(|queued| queued != &self.shared.operation_id);
                scheduler_inner.waiters.remove(&self.shared.operation_id);
                state.events.push_back(AgentEvent::CancellationRequested {
                    operation_id: self.shared.operation_id.clone(),
                    reason: CancelReason::User,
                });
                state.events.push_back(AgentEvent::OperationSettled {
                    operation_id: self.shared.operation_id.clone(),
                    status: OperationStatus::Cancelled,
                    durability: SettlementDurability::Confirmed,
                });
                state.phase = OperationPhase::Settled(OperationStatus::Cancelled);
                state.outcome = Some(OperationOutcome::Cancelled);
                let operation_waker = state.waker.take();
                let queue_waker =
                    if scheduler_inner.active.is_none() && scheduler_inner.maintenance.is_none() {
                        scheduler_inner
                            .queue
                            .front()
                            .and_then(|queued| scheduler_inner.waiters.get(queued))
                            .cloned()
                    } else {
                        None
                    };
                drop(state);
                drop(scheduler_inner);
                if let Some(waker) = operation_waker {
                    waker.wake();
                }
                if let Some(waker) = queue_waker {
                    waker.wake();
                }
            }
            _ => {
                self.shared.request_cancel(CancelReason::User);
                // Wake the consumer so an idle engine re-polls and observes
                // the request promptly (effect points stay the M6 ones).
                if let Some(waker) = state.waker.take() {
                    drop(state);
                    drop(scheduler_inner);
                    waker.wake();
                }
            }
        }
    }

    /// Drives the engine one step within the caller's context.
    fn drive(&self, cx: &mut std::task::Context<'_>) {
        let mut slot = self.engine.lock().expect("engine mutex");
        if let Some(engine) = slot.as_mut()
            && engine.as_mut().poll(cx).is_ready()
        {
            *slot = None;
        }
    }

    /// Returns the next published event as soon as it is available (M10
    /// real-time obligation), driving the operation as needed. `None` after
    /// the terminal event: the operation settled and the queue drained.
    pub async fn next_event(&mut self) -> Option<AgentEvent> {
        std::future::poll_fn(|cx| {
            use std::task::Poll;
            if let Some(event) = self.shared.pop_event() {
                return Poll::Ready(Some(event));
            }
            if self.shared.settled_outcome().is_some() {
                return Poll::Ready(None);
            }
            self.drive(cx);
            if let Some(event) = self.shared.pop_event() {
                return Poll::Ready(Some(event));
            }
            if self.shared.settled_outcome().is_some() {
                return Poll::Ready(None);
            }
            self.shared.register_waker(cx);
            Poll::Pending
        })
        .await
    }

    /// Drives this operation as needed and returns its terminal outcome.
    /// Waiting without consuming events still drives the work to settled;
    /// published events stay queued for `next_event`.
    pub async fn wait(&self) -> OperationOutcome {
        std::future::poll_fn(|cx| {
            use std::task::Poll;
            if let Some(outcome) = self.shared.settled_outcome() {
                return Poll::Ready(outcome);
            }
            self.drive(cx);
            match self.shared.settled_outcome() {
                Some(outcome) => Poll::Ready(outcome),
                None => {
                    self.shared.register_waker(cx);
                    Poll::Pending
                }
            }
        })
        .await
    }
}
