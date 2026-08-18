//! Monotonic shutdown signal. Must not share a mailbox with ordinary control.

use std::time::Instant;

use crate::{ShutdownError, ShutdownMode, ShutdownReport};

/// Request observed by the coordinator and the epoch supervisor.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ShutdownRequest {
    pub mode: ShutdownMode,
    pub deadline: Instant,
}

/// Outcome written once by the epoch supervisor.
pub(crate) type ShutdownOutcome = Result<ShutdownReport, ShutdownError>;

pub(crate) fn merge_shutdown(
    current: Option<ShutdownRequest>,
    incoming: ShutdownRequest,
) -> ShutdownRequest {
    match current {
        None => incoming,
        Some(existing) => ShutdownRequest {
            mode: stronger_mode(existing.mode, incoming.mode),
            deadline: existing.deadline.min(incoming.deadline),
        },
    }
}

fn stronger_mode(left: ShutdownMode, right: ShutdownMode) -> ShutdownMode {
    match (left, right) {
        (ShutdownMode::Forced, _) | (_, ShutdownMode::Forced) => ShutdownMode::Forced,
        (ShutdownMode::Drain, ShutdownMode::Drain) => ShutdownMode::Drain,
    }
}

pub(crate) fn default_shutdown_deadline() -> Instant {
    Instant::now() + std::time::Duration::from_secs(5)
}
