//! Bounded reliable staging between coordinator emit and the public event outlet.

use crate::RuntimeEvent;
use std::collections::VecDeque;

/// Worst-case reliable events from one producer handler (admit, queued
/// cancel, or driver-join settlement + fault).
pub(crate) const PRODUCER_STAGING_RESERVE: usize = 2;

/// FIFO of reliable events with a hard capacity from [`crate::ChannelBounds`].
pub(crate) struct ReliableStaging {
    cap: usize,
    queue: VecDeque<RuntimeEvent>,
}

impl ReliableStaging {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            cap,
            queue: VecDeque::new(),
        }
    }

    pub(crate) fn cap(&self) -> usize {
        self.cap
    }

    pub(crate) fn len(&self) -> usize {
        self.queue.len()
    }

    pub(crate) fn remaining(&self) -> usize {
        self.cap.saturating_sub(self.queue.len())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub(crate) fn can_accept_producer(&self) -> bool {
        self.remaining() >= PRODUCER_STAGING_RESERVE
    }

    pub(crate) fn push(&mut self, event: RuntimeEvent) -> Result<(), RuntimeEvent> {
        if self.queue.len() >= self.cap {
            return Err(event);
        }
        self.queue.push_back(event);
        Ok(())
    }

    pub(crate) fn pop_front(&mut self) -> Option<RuntimeEvent> {
        self.queue.pop_front()
    }

    pub(crate) fn push_front(&mut self, event: RuntimeEvent) {
        debug_assert!(self.queue.len() < self.cap);
        self.queue.push_front(event);
    }

    pub(crate) fn clear(&mut self) {
        self.queue.clear();
    }

    pub(crate) fn drain(&mut self) -> Vec<RuntimeEvent> {
        self.queue.drain(..).collect()
    }
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
        let mut staging = ReliableStaging::new(1);
        assert!(staging.push(availability()).is_ok());
        assert_eq!(staging.remaining(), 0);
        assert!(staging.push(availability()).is_err());
        assert_eq!(staging.len(), 1);
    }

    #[test]
    fn fifo_and_clear() {
        let mut staging = ReliableStaging::new(2);
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
        let mut staging = ReliableStaging::new(2);
        assert!(staging.can_accept_producer());
        assert!(staging.push(availability()).is_ok());
        assert!(!staging.can_accept_producer());
    }
}
