//! Admission snapshots for operations and maintenance.

use crate::{MaintenanceId, OperationId, RuntimeGeneration, SessionId, TurnId, UserMessage};
use std::sync::Arc;

/// Complete, immutable admission snapshot for one user operation.
pub struct OperationSpec {
    pub session_id: SessionId,
    pub user_message: UserMessage,
    pub generation: Arc<RuntimeGeneration>,
    /// Correlation id for the service layer; never persisted to Session.
    pub service_request_id: Option<String>,
}

/// Admission snapshot for one manual compaction.
pub struct CompactionSpec {
    pub session_id: SessionId,
    pub generation: Arc<RuntimeGeneration>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationAccepted {
    pub operation_id: OperationId,
    pub turn_id: TurnId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaintenanceAccepted {
    pub id: MaintenanceId,
}
