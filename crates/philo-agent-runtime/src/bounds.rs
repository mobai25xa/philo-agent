//! Hard caps for runtime channels and the in-memory FIFO.

pub const RUNTIME_COMMAND_CAP: usize = 32;
pub const RUNTIME_CONTROL_CAP: usize = 16;
pub const RUNTIME_EVENT_CAP: usize = 256;
pub const RUNTIME_QUEUE_MAX: usize = 32;
pub const RUNTIME_DRIVER_EVENT_BUDGET: usize = 32;
pub const DELTA_MERGE_CHUNK_MAX: usize = 4096;

/// Channel and queue capacities frozen when the runtime starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelBounds {
    pub command_cap: usize,
    pub control_cap: usize,
    pub event_cap: usize,
    pub queue_max: usize,
    pub driver_event_budget: usize,
}

impl Default for ChannelBounds {
    fn default() -> Self {
        Self {
            command_cap: RUNTIME_COMMAND_CAP,
            control_cap: RUNTIME_CONTROL_CAP,
            event_cap: RUNTIME_EVENT_CAP,
            queue_max: RUNTIME_QUEUE_MAX,
            driver_event_budget: RUNTIME_DRIVER_EVENT_BUDGET,
        }
    }
}

impl ChannelBounds {
    pub(crate) fn validate(self) -> Result<Self, super::StartError> {
        if self.command_cap == 0
            || self.control_cap == 0
            || self.event_cap == 0
            || self.driver_event_budget == 0
        {
            return Err(super::StartError::InvalidBounds);
        }
        Ok(self)
    }
}
