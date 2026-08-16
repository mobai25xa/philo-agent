//! Runtime coordinator for direct answers and the bounded multi-round tool loop.

mod compaction;
mod config;
mod engine;
mod event;
mod ids;
mod mapping;
mod message;
mod model;
mod operation;
mod outcome;
mod runtime;
mod snapshot;

pub use compaction::{CompactionError, CompactionReport};
pub use config::{
    CompactionConfig, DEFAULT_MAX_PARALLEL_TOOL_CALLS, DEFAULT_MAX_TOOL_ROUNDS, GenerationConfig,
    ReasoningEffort, RuntimeConfig, ToolChoice,
};
pub use event::AgentEvent;
pub use ids::{
    IdSource, ModelCallId, OperationId, SequentialIdSource, SessionId, ToolBatchId, ToolCallId,
    TurnId,
};
pub use message::{AssistantMessage, InvalidUserMessage, UserMessage, UserPart};
pub use model::{
    ModelError, ModelEvent, ModelEventStream, ModelMessage, ModelPort, ModelToolCall,
    ModelToolResultOutcome, RuntimeFuture, TokenUsage, ToolCallDelta,
};
pub use operation::OperationHandle;
pub use outcome::{
    AgentAvailability, AgentError, AgentFailure, AgentFailureKind, ModelCallPhase,
    OperationOutcome, OperationPhase, OperationStatus, RunningToolBatchPhase, SettlementDurability,
};
pub use philo_session::CancelReason;
pub use philo_tools::{
    EffectClass, RichToolResult, ToolArguments, ToolDefinition, ToolDisplay, ToolFact, ToolFuture,
    ToolHandler, ToolHandlerFuture, ToolInvocation, ToolPort, ToolPortError, ToolRegistry,
    ToolRegistryBuilder, ToolResult, ToolResultError, ToolSchema, ToolSchemaInput,
};
pub use runtime::AgentRuntime;
pub use snapshot::{ModelCallSnapshot, TurnSnapshot};
