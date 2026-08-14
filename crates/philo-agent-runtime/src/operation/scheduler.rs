//! Single-active-operation scheduling with a FIFO follow-up queue.

use super::shared::OperationShared;
use crate::{AgentAvailability, OperationId, OperationPhase};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Single-active-operation scheduler with a FIFO follow-up queue.
///
/// The queue is process-local and never persisted: a crash drops it.
pub(crate) struct Scheduler {
    pub(super) inner: Mutex<SchedulerInner>,
}

pub(super) struct SchedulerInner {
    pub(super) active: Option<OperationId>,
    pub(super) queue: VecDeque<OperationId>,
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
                queue: VecDeque::new(),
            }),
        })
    }

    /// Admits a new operation: claims the active slot when the runtime is
    /// fully idle, otherwise appends to the FIFO queue.
    pub fn admit(&self, operation_id: &OperationId) -> Admission {
        let mut inner = self.inner.lock().expect("scheduler mutex");
        if inner.active.is_none() && inner.queue.is_empty() {
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
    pub fn try_claim_queued(&self, shared: &OperationShared) -> QueueClaim {
        let mut scheduler = self.inner.lock().expect("scheduler mutex");
        let mut state = shared.inner.lock().expect("operation mutex");
        if state.outcome.is_some() {
            return QueueClaim::SettledInQueue;
        }
        if scheduler.active.is_none() && scheduler.queue.front() == Some(&shared.operation_id) {
            scheduler.queue.pop_front();
            scheduler.active = Some(shared.operation_id.clone());
            state.phase = OperationPhase::PreparingTurn;
            QueueClaim::Claimed
        } else {
            QueueClaim::NotYet
        }
    }

    pub fn release(&self, operation_id: &OperationId) {
        let mut inner = self.inner.lock().expect("scheduler mutex");
        if inner.active.as_ref() == Some(operation_id) {
            inner.active = None;
        }
    }

    pub fn availability(&self) -> AgentAvailability {
        let inner = self.inner.lock().expect("scheduler mutex");
        match &inner.active {
            Some(operation_id) => AgentAvailability::Busy {
                operation_id: operation_id.clone(),
            },
            None => AgentAvailability::Idle,
        }
    }
}
