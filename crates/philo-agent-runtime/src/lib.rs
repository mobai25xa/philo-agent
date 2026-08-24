//! Runtime coordinator for direct answers and the bounded multi-round tool loop.

mod bounds;
mod catch_unwind;
mod compaction;
mod config;
mod coordinator;
mod engine;
mod epoch;
mod error;
mod event;
mod generation;
mod handle;
mod ids;
mod mapping;
mod message;
mod model;
mod operation;
mod outcome;
mod runtime;
mod runtime_event;
mod shutdown;
mod snapshot;
mod spec;
mod staging;
mod subscription;
mod transient;

pub use bounds::{
    ChannelBounds, DELTA_MERGE_CHUNK_MAX, RUNTIME_COMMAND_CAP, RUNTIME_CONTROL_CAP,
    RUNTIME_DRIVER_EVENT_BUDGET, RUNTIME_EVENT_CAP, RUNTIME_QUEUE_MAX,
    RUNTIME_RELIABLE_STAGING_CAP, TRANSIENT_KIND_COUNT,
};
pub use compaction::{CompactionError, CompactionReport};
pub use config::{
    CompactionConfig, DEFAULT_MAX_PARALLEL_TOOL_CALLS, DEFAULT_MAX_TOOL_ROUNDS,
    DEFAULT_TOOL_CANCEL_GRACE, GenerationConfig, ReasoningEffort, RecoveryConfig, RuntimeConfig,
    ToolChoice,
};
pub use error::{
    AdmissionError, CancelResult, DriverExit, DriverInvariantError, ForcedSettlement,
    MaintenanceError, ShutdownDiagnostic, ShutdownError, StartError,
};
pub use event::AgentEvent;
pub use generation::{GenerationDisplay, RuntimeGeneration};
pub use handle::RuntimeHandle;
pub use ids::{
    DiagnosticId, GenerationId, IdSource, MaintenanceId, ModelCallId, OperationId, RuntimeEpoch,
    SequentialIdSource, SessionId, ToolBatchId, ToolCallId, TurnId,
};
pub use message::{AssistantMessage, InvalidUserMessage, UserMessage, UserPart};
pub use model::{
    ModelAssistantBlock, ModelError, ModelEvent, ModelEventStream, ModelMessage, ModelPort,
    ModelToolCall, ModelToolResultOutcome, RuntimeFuture, TokenUsage, ToolCallDelta,
};
pub use outcome::{
    AgentAvailability, AgentError, AgentFailure, DurableFailureKind, FailureDomain, FailureStage,
    ModelCallPhase, OperationOutcome, OperationPhase, OperationStatus, RetryDisposition,
    RunningToolBatchPhase, SettlementDurability, SettlementRevision, AGENT_OWNED_CODES,
};
pub use philo_session::CancelReason;
pub use philo_tools::{
    EffectClass, RichToolResult, ToolArguments, ToolCancel, ToolDefinition, ToolDisplay, ToolFact,
    ToolFuture, ToolHandler, ToolHandlerEndFuture, ToolHandlerFuture, ToolInvocation, ToolInvokeCx,
    ToolInvokeEnd, ToolPort, ToolPortError, ToolProgressSink, ToolRegistry, ToolRegistryBuilder,
    ToolResult, ToolResultError, ToolSchema, ToolSchemaInput,
};
pub use runtime::{AgentRuntime, RuntimeDeps, RuntimeParts};
pub use runtime_event::{EpochEndReason, MaintenanceResult, RuntimeEvent, TryRecvError};
pub use snapshot::{
    ActiveOperationSnapshot, MaintenanceSnapshot, ModelCallSnapshot, QueuedOperationSnapshot,
    RuntimeSnapshot, SettledOperationSnapshot, ShutdownMode, ShutdownReport, ShutdownState,
    TurnSnapshot,
};
pub use spec::{CompactionSpec, MaintenanceAccepted, OperationAccepted, OperationSpec};
pub use staging::OutboundStats;
pub use subscription::RuntimeEventReceiver;
