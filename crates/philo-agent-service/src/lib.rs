//! Application service between Runtime and frontends.
//!
//! The TUI talks only to [`FrontendClient`]. This crate owns command identity,
//! bounded live/control state, snapshot composition, generation install, and
//! confirmation. It does **not** store a second full transcript.

mod actor;
mod bounds;
mod confirmation;
mod error;
mod frontend;
mod generation;
mod ids;
mod live;
mod mapping;
mod runtime_api;
mod service;

pub mod testing;

pub use bounds::{
    BLOCKING_TOOL_QUEUE, CONFIRMATION_MAP_CAP, DELTA_MERGE_CHUNK_MAX, FRONTEND_COMMAND_CAP,
    FRONTEND_CONTROL_CAP, FRONTEND_RESTART_BUDGET, FRONTEND_RESTART_WINDOW_SECS,
    FRONTEND_SNAPSHOT_CAP, FRONTEND_UPDATE_CAP, LIVE_REASONING_CHARS_MAX, LIVE_TEXT_CHARS_MAX,
    LIVE_TOOL_PROGRESS_MAX, RUNTIME_COMMAND_CAP, RUNTIME_CONTROL_CAP, RUNTIME_DRIVER_EVENT_BUDGET,
    RUNTIME_EVENT_CAP, RUNTIME_QUEUE_MAX, RUNTIME_RELIABLE_STAGING_CAP, STORE_COMMAND_CAP,
    TRANSIENT_KIND_COUNT,
};
pub use confirmation::{ConfirmationGate, ConfirmationRequest};
pub use error::{CommandDispatch, CommandReject, RecvOutcome, ServiceError};
pub use frontend::{
    AttachError, ConfirmationDecision, DetachError, DetachReason, DetachReport, DurableSessionView,
    FrontendAssistantBlock, FrontendAttachment, FrontendAvailability, FrontendClient,
    FrontendCommand, FrontendConfigEntry, FrontendContextMessage, FrontendGeneration,
    FrontendLease, FrontendLeaseGeneration, FrontendMaintenance, FrontendMaintenancePhase,
    FrontendOpenTurn, FrontendOperationEvent, FrontendReasoningEffort, FrontendSnapshot,
    FrontendStatus, FrontendTokenUsage, FrontendToolDisplay, FrontendToolListing,
    FrontendToolResult, FrontendToolResultOutcome, FrontendUnfilledBatch, FrontendUpdate,
    FrontendUpdateKind, FrontendUserPart, PendingConfirmationView, QueuedOperationSummary,
    ResyncRequired, ServiceHealth, SupervisorCommand,
};
pub use generation::{AssembleError, AssembleRequest, AssembledGeneration, GenerationAssembler};
pub use ids::{FrontendEpoch, FrontendInstanceId, FrontendRequestId, FrontendRevision};
pub use live::{LiveOperationSnapshot, LiveToolProgress};
pub use philo_agent_runtime::{
    AdmissionError, CancelResult, ChannelBounds, CompactionSpec, ForcedSettlement,
    GenerationDisplay, GenerationId, MaintenanceAccepted, MaintenanceError, MaintenanceId,
    MaintenanceResult, OperationAccepted, OperationSpec, QueuedOperationSnapshot, RuntimeEvent,
    RuntimeEventReceiver, RuntimeGeneration, RuntimeHandle, RuntimeParts, RuntimeSnapshot,
    SettlementRevision, ShutdownError, ShutdownMode, ShutdownReport, TryRecvError,
};
pub use runtime_api::{RuntimeEvents, RuntimePort};
pub use service::{AgentService, ServiceDeps, start};
