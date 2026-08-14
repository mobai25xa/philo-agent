//! Process-local runtime IDs and filesystem-friendly session IDs.

use std::sync::atomic::{AtomicU64, Ordering};

use philo_agent_runtime::{IdSource, OperationId, TurnId};

/// IDs for one CLI process. The random-enough run prefix prevents collisions
/// across processes; counters keep every admitted operation unique within a
/// long-lived interactive process.
pub struct ProcessIdSource {
    run_id: String,
    operations: AtomicU64,
    turns: AtomicU64,
}

impl ProcessIdSource {
    pub fn new() -> Self {
        Self {
            run_id: fresh_session_id(),
            operations: AtomicU64::new(0),
            turns: AtomicU64::new(0),
        }
    }
}

impl Default for ProcessIdSource {
    fn default() -> Self {
        Self::new()
    }
}

impl IdSource for ProcessIdSource {
    fn next_operation_id(&self) -> OperationId {
        let index = self.operations.fetch_add(1, Ordering::Relaxed);
        OperationId::new(format!("{}-op-{index}", self.run_id))
    }

    fn next_turn_id(&self) -> TurnId {
        let index = self.turns.fetch_add(1, Ordering::Relaxed);
        TurnId::new(format!("{}-turn-{index}", self.run_id))
    }
}

/// Generates a filesystem-encoding-friendly fresh session id.
pub fn fresh_session_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    format!("sess-{millis:x}-{}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_session_ids_are_filesystem_friendly() {
        let id = fresh_session_id();
        assert!(id.starts_with("sess-"));
        assert!(
            id.bytes()
                .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-'))
        );
    }

    #[test]
    fn process_ids_are_unique_and_share_the_run_prefix() {
        let ids = ProcessIdSource::new();
        let first = ids.next_operation_id();
        let second = ids.next_operation_id();
        assert_ne!(first, second);
        assert!(first.as_str().ends_with("-op-0"));
        assert!(second.as_str().ends_with("-op-1"));
    }
}
