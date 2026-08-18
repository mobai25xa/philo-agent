//! Bounded reliable staging between coordinator emit and the public event outlet.

use crate::RuntimeEvent;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Worst-case reliable events from one producer handler (admit, queued
/// cancel, or driver-join settlement + fault).
pub(crate) const PRODUCER_STAGING_RESERVE: usize = 2;

struct StagingInner {
    cap: usize,
    queue: VecDeque<RuntimeEvent>,
}

/// FIFO of reliable events with a hard capacity from [`crate::ChannelBounds`].
///
/// Shared between the coordinator and the epoch supervisor so a panic cannot
/// drop events that already entered staging.
#[derive(Clone)]
pub(crate) struct ReliableStaging {
    inner: Arc<Mutex<StagingInner>>,
}

impl ReliableStaging {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(StagingInner {
                cap,
                queue: VecDeque::new(),
            })),
        }
    }

    pub(crate) fn cap(&self) -> usize {
        lock(&self.inner).cap
    }

    pub(crate) fn len(&self) -> usize {
        lock(&self.inner).queue.len()
    }

    pub(crate) fn remaining(&self) -> usize {
        let inner = lock(&self.inner);
        inner.cap.saturating_sub(inner.queue.len())
    }

    pub(crate) fn is_empty(&self) -> bool {
        lock(&self.inner).queue.is_empty()
    }

    pub(crate) fn can_accept_producer(&self) -> bool {
        self.remaining() >= PRODUCER_STAGING_RESERVE
    }

    pub(crate) fn push(&self, event: RuntimeEvent) -> Result<(), RuntimeEvent> {
        let mut inner = lock(&self.inner);
        if inner.queue.len() >= inner.cap {
            return Err(event);
        }
        inner.queue.push_back(event);
        Ok(())
    }

    pub(crate) fn pop_front(&self) -> Option<RuntimeEvent> {
        lock(&self.inner).queue.pop_front()
    }

    pub(crate) fn push_front(&self, event: RuntimeEvent) {
        let mut inner = lock(&self.inner);
        debug_assert!(inner.queue.len() < inner.cap);
        inner.queue.push_front(event);
    }

    pub(crate) fn clear(&self) {
        lock(&self.inner).queue.clear();
    }

    pub(crate) fn drain(&self) -> Vec<RuntimeEvent> {
        lock(&self.inner).queue.drain(..).collect()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Test-visible occupancy of the two coordinator outbound buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub struct OutboundStats {
    pub reliable_staging_len: usize,
    pub reliable_staging_cap: usize,
    pub transient_len: usize,
    pub transient_cap: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentAvailability, RuntimeEvent};

    fn availability() -> RuntimeEvent {
        RuntimeEvent::AvailabilityChanged {
            availability: AgentAvailability::Idle,
            queued: 0,
        }
    }

    #[test]
    fn push_rejects_when_full() {
        let staging = ReliableStaging::new(1);
        assert!(staging.push(availability()).is_ok());
        assert_eq!(staging.remaining(), 0);
        assert!(staging.push(availability()).is_err());
        assert_eq!(staging.len(), 1);
    }

    #[test]
    fn fifo_and_clear() {
        let staging = ReliableStaging::new(2);
        let first = RuntimeEvent::AvailabilityChanged {
            availability: AgentAvailability::Idle,
            queued: 1,
        };
        let second = RuntimeEvent::AvailabilityChanged {
            availability: AgentAvailability::Idle,
            queued: 2,
        };
        assert!(staging.push(first.clone()).is_ok());
        assert!(staging.push(second.clone()).is_ok());
        assert_eq!(staging.pop_front(), Some(first));
        assert_eq!(staging.pop_front(), Some(second));
        assert!(staging.is_empty());
        assert!(staging.push(availability()).is_ok());
        staging.clear();
        assert!(staging.is_empty());
        assert_eq!(staging.remaining(), 2);
    }

    #[test]
    fn producer_reserve_requires_two_slots() {
        let staging = ReliableStaging::new(2);
        assert!(staging.can_accept_producer());
        assert!(staging.push(availability()).is_ok());
        assert!(!staging.can_accept_producer());
    }

    #[test]
    fn clone_shares_the_same_queue() {
        let staging = ReliableStaging::new(2);
        let other = staging.clone();
        assert!(staging.push(availability()).is_ok());
        assert_eq!(other.len(), 1);
        assert_eq!(other.drain().len(), 1);
        assert!(staging.is_empty());
    }
}
