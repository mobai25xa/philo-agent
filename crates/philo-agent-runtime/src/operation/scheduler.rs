//! Single-active-operation scheduling with a FIFO follow-up queue.

use super::shared::OperationShared;
use crate::{AgentAvailability, OperationId, OperationPhase, SessionId};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::task::Waker;

/// Single-active-operation scheduler with a FIFO follow-up queue.
///
/// The queue is process-local and never persisted: a crash drops it.
pub(crate) struct Scheduler {
    pub(super) inner: Mutex<SchedulerInner>,
}

pub(super) struct SchedulerInner {
    pub(super) active: Option<OperationId>,
    pub(super) maintenance: Option<SessionId>,
    pub(super) queue: VecDeque<OperationId>,
    pub(super) waiters: HashMap<OperationId, Waker>,
}

/// Exclusive scheduler claim for non-operation maintenance. Dropping a
/// pending `compact()` future releases the claim and wakes the FIFO head.
pub(crate) struct MaintenanceLease {
    scheduler: Arc<Scheduler>,
    session_id: SessionId,
}

impl Drop for MaintenanceLease {
    fn drop(&mut self) {
        self.scheduler.release_maintenance(&self.session_id);
    }
}

pub(crate) enum Admission {
    /// The caller may drive immediately.
    Direct,
    /// The operation waits in the FIFO queue.
    Queued,
}

pub(crate) enum QueueClaim {
    Claimed,
    NotYet,
    SettledInQueue,
}

impl Scheduler {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(SchedulerInner {
                active: None,
                maintenance: None,
                queue: VecDeque::new(),
                waiters: HashMap::new(),
            }),
        })
    }

    /// Admits a new operation: claims the active slot when the runtime is
    /// fully idle, otherwise appends to the FIFO queue.
    pub fn admit(&self, operation_id: &OperationId) -> Admission {
        let mut inner = self.inner.lock().expect("scheduler mutex");
        if inner.active.is_none() && inner.maintenance.is_none() && inner.queue.is_empty() {
            inner.active = Some(operation_id.clone());
            Admission::Direct
        } else {
            inner.queue.push_back(operation_id.clone());
            Admission::Queued
        }
    }

    /// Atomically claims the active slot for the queue head. Also settles the
    /// race with `OperationHandle::cancel`: a queued operation cancelled in
    /// the same instant is observed as settled, never double-driven.
    pub fn try_claim_queued(&self, shared: &OperationShared, waker: &Waker) -> QueueClaim {
        let mut scheduler = self.inner.lock().expect("scheduler mutex");
        let mut state = shared.inner.lock().expect("operation mutex");
        if state.outcome.is_some() {
            return QueueClaim::SettledInQueue;
        }
        if scheduler.active.is_none()
            && scheduler.maintenance.is_none()
            && scheduler.queue.front() == Some(&shared.operation_id)
        {
            scheduler.queue.pop_front();
            scheduler.waiters.remove(&shared.operation_id);
            scheduler.active = Some(shared.operation_id.clone());
            state.phase = OperationPhase::PreparingTurn;
            QueueClaim::Claimed
        } else {
            scheduler
                .waiters
                .insert(shared.operation_id.clone(), waker.clone());
            QueueClaim::NotYet
        }
    }

    pub fn release(&self, operation_id: &OperationId) {
        let waker = {
            let mut inner = self.inner.lock().expect("scheduler mutex");
            if inner.active.as_ref() == Some(operation_id) {
                inner.active = None;
            }
            next_waiter(&inner)
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Atomically acquires the idle scheduler slot for manual maintenance.
    pub fn acquire_maintenance(
        self: &Arc<Self>,
        session_id: &SessionId,
    ) -> Result<MaintenanceLease, AgentAvailability> {
        let mut inner = self.inner.lock().expect("scheduler mutex");
        if inner.active.is_none() && inner.maintenance.is_none() && inner.queue.is_empty() {
            inner.maintenance = Some(session_id.clone());
            return Ok(MaintenanceLease {
                scheduler: self.clone(),
                session_id: session_id.clone(),
            });
        }
        Err(availability_from(&inner))
    }

    fn release_maintenance(&self, session_id: &SessionId) {
        let waker = {
            let mut inner = self.inner.lock().expect("scheduler mutex");
            if inner.maintenance.as_ref() == Some(session_id) {
                inner.maintenance = None;
            }
            next_waiter(&inner)
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub fn availability(&self) -> AgentAvailability {
        let inner = self.inner.lock().expect("scheduler mutex");
        availability_from(&inner)
    }
}

fn availability_from(inner: &SchedulerInner) -> AgentAvailability {
    if let Some(session_id) = &inner.maintenance {
        AgentAvailability::Compacting {
            session_id: session_id.clone(),
        }
    } else if let Some(operation_id) = &inner.active {
        AgentAvailability::Busy {
            operation_id: operation_id.clone(),
        }
    } else {
        AgentAvailability::Idle
    }
}

fn next_waiter(inner: &SchedulerInner) -> Option<Waker> {
    if inner.active.is_some() || inner.maintenance.is_some() {
        return None;
    }
    inner
        .queue
        .front()
        .and_then(|operation_id| inner.waiters.get(operation_id))
        .cloned()
}
