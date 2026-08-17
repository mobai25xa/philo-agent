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
mod snapshot;
mod spec;
mod subscription;
mod transient;

pub use bounds::{
    ChannelBounds, DELTA_MERGE_CHUNK_MAX, RUNTIME_COMMAND_CAP, RUNTIME_CONTROL_CAP,
    RUNTIME_DRIVER_EVENT_BUDGET, RUNTIME_EVENT_CAP, RUNTIME_QUEUE_MAX,
};
pub use compaction::{CompactionError, CompactionReport};
pub use config::{
    CompactionConfig, DEFAULT_MAX_PARALLEL_TOOL_CALLS, DEFAULT_MAX_TOOL_ROUNDS,
    DEFAULT_TOOL_CANCEL_GRACE, GenerationConfig, ReasoningEffort, RuntimeConfig, ToolChoice,
};
pub use error::{
    AdmissionError, CancelResult, DriverExit, DriverInvariantError, EpochSettlement,
    MaintenanceError, StartError,
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
    AgentAvailability, AgentError, AgentFailure, AgentFailureKind, ModelCallPhase,
    OperationOutcome, OperationPhase, OperationStatus, RunningToolBatchPhase, SettlementDurability,
};
pub use philo_session::CancelReason;
pub use philo_tools::{
    EffectClass, RichToolResult, ToolArguments, ToolCancel, ToolDefinition, ToolDisplay, ToolFact,
    ToolFuture, ToolHandler, ToolHandlerEndFuture, ToolHandlerFuture, ToolInvocation, ToolInvokeCx,
    ToolInvokeEnd, ToolPort, ToolPortError, ToolProgressSink, ToolRegistry, ToolRegistryBuilder,
    ToolResult, ToolResultError, ToolSchema, ToolSchemaInput,
};
pub use runtime::{AgentRuntime, RuntimeDeps};
pub use runtime_event::{MaintenanceResult, RuntimeEvent, TryRecvError};
pub use snapshot::{
    ActiveOperationSnapshot, MaintenanceSnapshot, ModelCallSnapshot, RuntimeSnapshot,
    SettledOperationSnapshot, ShutdownMode, ShutdownReport, ShutdownState, TurnSnapshot,
};
pub use spec::{CompactionSpec, MaintenanceAccepted, OperationAccepted, OperationSpec};
pub use subscription::RuntimeSubscription;
