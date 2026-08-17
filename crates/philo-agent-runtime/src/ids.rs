//! Identifier vocabulary shared across the runtime, plus the ID source port.

use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        pub struct $name(String);
        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}
string_id!(SessionId);
string_id!(OperationId);
string_id!(TurnId);
string_id!(ModelCallId);
string_id!(ToolBatchId);
string_id!(ToolCallId);
string_id!(GenerationId);
string_id!(MaintenanceId);
string_id!(RuntimeEpoch);
string_id!(DiagnosticId);

pub trait IdSource: Send + Sync {
    fn next_operation_id(&self) -> OperationId;
    fn next_turn_id(&self) -> TurnId;
}

#[derive(Debug, Default)]
pub struct SequentialIdSource {
    next_operation: AtomicU64,
    next_turn: AtomicU64,
}
impl SequentialIdSource {
    pub fn new() -> Self {
        Self::default()
    }
}
impl IdSource for SequentialIdSource {
    fn next_operation_id(&self) -> OperationId {
        OperationId::new(format!(
            "operation-{}",
            self.next_operation.fetch_add(1, Ordering::Relaxed) + 1
        ))
    }
    fn next_turn_id(&self) -> TurnId {
        TurnId::new(format!(
            "turn-{}",
            self.next_turn.fetch_add(1, Ordering::Relaxed) + 1
        ))
    }
}
