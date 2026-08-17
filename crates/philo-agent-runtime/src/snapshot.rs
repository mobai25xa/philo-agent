//! Frozen per-turn and per-call facts handed to the model port, plus the
//! coordinator's observable runtime snapshot.

use crate::{
    AgentAvailability, AgentFailure, GenerationConfig, MaintenanceId, ModelCallId, ModelMessage,
    OperationId, OperationPhase, OperationStatus, RuntimeEpoch, SessionId, SettlementDurability,
    TurnId,
};
use philo_session::SessionRevision;
use philo_tools::ToolDefinition;

#[derive(Clone, Debug, PartialEq)]
pub struct TurnSnapshot {
    pub session_id: SessionId,
    pub session_revision: SessionRevision,
    pub context_messages: Vec<ModelMessage>,
    pub system_prompt: String,
    pub model_target: String,
    pub generation: GenerationConfig,
    pub tools: Vec<ToolDefinition>,
    pub max_tool_rounds: u32,
    /// Concurrent invoke cap frozen for this turn; `1` is the serial path.
    pub max_parallel_tool_calls: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelCallSnapshot {
    /// Session owning this call. Model adapters use it to scope provider
    /// replay sidecars and, for openai-chat, as the stable cache-routing
    /// identity (`prompt_cache_key` / session-affinity headers).
    pub session_id: SessionId,
    /// Opaque active-path leaf before this call. It prevents replay state
    /// from one context branch being treated as state for another branch.
    pub context_fingerprint: String,
    /// Whether this call produces assistant history that must be persisted.
    /// Synthetic maintenance calls set this to false.
    pub persist_replay: bool,
    pub operation_id: OperationId,
    pub turn_id: TurnId,
    pub model_call_id: ModelCallId,
    /// One-based sequence number of this logical call within the turn.
    pub model_call_index: u32,
    pub session_revision: SessionRevision,
    pub messages: Vec<ModelMessage>,
    /// Frozen tool definitions when the kernel allows tools, empty otherwise.
    pub tools: Vec<ToolDefinition>,
    pub model_target: String,
    pub generation: GenerationConfig,
    /// Frozen concurrent invoke cap; adapters send `Allow` when this is `> 1`.
    pub max_parallel_tool_calls: u32,
}

/// Live observation of the coordinator. Updated in the same actor turn as
/// Started/Settled transitions. Absence of `Started` on a snapshot is not
/// a crash-recovery proof that a queued operation never began.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    pub epoch: RuntimeEpoch,
    pub availability: AgentAvailability,
    pub queued: Vec<OperationId>,
    pub active: Option<ActiveOperationSnapshot>,
    pub maintenance: Option<MaintenanceSnapshot>,
    pub shutdown: ShutdownState,
    pub last_settled: Vec<SettledOperationSnapshot>,
    /// Monotonic coordinator publication counter. Bumped on every snapshot.
    pub runtime_revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveOperationSnapshot {
    pub operation_id: OperationId,
    pub turn_id: TurnId,
    pub session_id: SessionId,
    pub phase: OperationPhase,
    pub started: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaintenanceSnapshot {
    pub id: MaintenanceId,
    pub session_id: SessionId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettledOperationSnapshot {
    pub operation_id: OperationId,
    pub status: OperationStatus,
    pub durability: SettlementDurability,
    pub failure: Option<AgentFailure>,
}

/// Coordinator shutdown mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownMode {
    Drain,
    Forced,
}

/// Observable shutdown state of one runtime epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownState {
    Running,
    Draining,
    Forced,
    Stopped,
}

/// Report returned by [`crate::RuntimeHandle::shutdown`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShutdownReport {
    pub epoch: RuntimeEpoch,
    pub shutdown: ShutdownState,
    pub settlements: Vec<crate::EpochSettlement>,
}
