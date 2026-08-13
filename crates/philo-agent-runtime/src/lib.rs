//! Runtime coordinator for direct answers and the bounded multi-round tool loop.

mod driver;
mod event;
mod model;
mod operation;
mod runtime;
mod snapshot;
mod tool;

pub use event::AgentEvent;
pub use model::{
    ModelError, ModelEvent, ModelEventStream, ModelMessage, ModelPort, ModelToolCall,
    ModelToolResultOutcome, RuntimeFuture, TokenUsage, ToolCallDelta,
};
pub use operation::{
    AgentAvailability, AgentError, AgentFailure, AgentFailureKind, AssistantMessage, IdSource,
    InvalidUserMessage, ModelCallId, ModelCallPhase, OperationHandle, OperationId,
    OperationOutcome, OperationPhase, OperationStatus, RunningToolBatchPhase, SequentialIdSource,
    SessionId, SettlementDurability, ToolBatchId, ToolCallId, TurnId, UserMessage, UserPart,
};
pub use philo_session::CancelReason;
pub use philo_tools::{
    EffectClass, RichToolResult, ToolArguments, ToolDefinition, ToolDisplay, ToolFact, ToolFuture,
    ToolHandler, ToolHandlerFuture, ToolInvocation, ToolPort, ToolPortError, ToolRegistry,
    ToolRegistryBuilder, ToolResult, ToolResultError, ToolSchema, ToolSchemaInput,
};
pub use runtime::AgentRuntime;
pub use snapshot::{
    DEFAULT_MAX_TOOL_ROUNDS, GenerationConfig, ModelCallSnapshot, ReasoningEffort, RuntimeConfig,
    ToolChoice, TurnSnapshot,
};
