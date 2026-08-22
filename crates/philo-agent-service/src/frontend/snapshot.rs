//! Frontend snapshot and the DTO vocabulary it embeds.
//!
//! A snapshot is Session durable view + bounded live/control state. It is
//! not a second transcript and not a ratatui buffer.

use crate::ids::{FrontendEpoch, FrontendRevision};
use crate::live::LiveOperationSnapshot;

/// Secret-free generation metadata shown to the frontend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendGeneration {
    /// Generation id.
    pub generation_id: String,
    /// User-facing model name.
    pub model_name: String,
    /// Frozen reasoning effort label, if any.
    pub reasoning_effort: Option<String>,
    /// Registered tool names at install time.
    pub tool_names: Vec<String>,
}

/// Composed input for frontend restart and resync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendSnapshot {
    /// Service/runtime epoch.
    pub epoch: FrontendEpoch,
    /// Service revision of this snapshot.
    pub revision: FrontendRevision,
    /// Current session, if any.
    pub current_session_id: Option<String>,
    /// Latest `SessionContextView` mapped at request time. `None` if no session.
    pub durable_session_view: Option<DurableSessionView>,
    /// Bounded live operation projection. Never a full event log.
    pub live: LiveOperationSnapshot,
    /// Queued operation summaries (capped).
    pub queued: Vec<QueuedOperationSummary>,
    /// Current maintenance, if any.
    pub maintenance: Option<FrontendMaintenance>,
    /// Availability.
    pub availability: FrontendAvailability,
    /// Current generation display metadata.
    pub generation: FrontendGeneration,
    /// Latest observed token usage.
    pub usage: Option<FrontendTokenUsage>,
    /// Confirmations still waiting.
    pub pending_confirmations: Vec<PendingConfirmationView>,
    /// Recent config/service notices.
    pub config_notices: Vec<String>,
    /// Service health.
    pub health: ServiceHealth,
}

/// Durable session projection mapped from `SessionContextView`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurableSessionView {
    /// Session id.
    pub session_id: String,
    /// Session store revision.
    pub revision: u64,
    /// Model-visible messages on the active path. Structured, unwrapped.
    pub messages: Vec<FrontendContextMessage>,
    /// Turns without a durable terminal outcome.
    pub open_turns: Vec<FrontendOpenTurn>,
    /// Opaque settled-turn boundary ids.
    pub settled_turn_boundaries: Vec<String>,
    /// Newest compaction boundary, if any.
    pub latest_compaction_boundary: Option<String>,
}

/// One model-visible context message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontendContextMessage {
    /// Durable summary of an earlier prefix.
    Summary {
        /// Summary text.
        text: String,
    },
    /// User message.
    User {
        /// Ordered parts.
        parts: Vec<FrontendUserPart>,
    },
    /// Final assistant message.
    Assistant {
        /// Ordered blocks.
        blocks: Vec<FrontendAssistantBlock>,
    },
    /// Assistant tool-call batch.
    AssistantToolCalls {
        /// Tool batch id.
        tool_batch_id: String,
        /// Ordered blocks.
        blocks: Vec<FrontendAssistantBlock>,
    },
    /// Tool result visible to a later model call.
    ToolResult {
        /// Tool call id.
        tool_call_id: String,
        /// Durable outcome.
        outcome: FrontendToolResultOutcome,
    },
}

/// One user-message part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontendUserPart {
    /// Text part.
    Text(String),
    /// Image bytes stored by Session.
    Image {
        /// MIME type.
        media_type: String,
        /// Raw bytes.
        bytes: Vec<u8>,
    },
}

/// One assistant block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontendAssistantBlock {
    /// Non-empty text.
    Text {
        /// Text.
        text: String,
    },
    /// A tool call.
    ToolCall {
        /// Tool call id.
        id: String,
        /// Tool name.
        name: String,
        /// Raw argument text.
        arguments: String,
    },
}

/// Durable tool-result outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontendToolResultOutcome {
    /// Success text.
    Success {
        /// Content.
        content: String,
    },
    /// Recoverable tool error.
    Error {
        /// Stable code.
        code: String,
        /// Message.
        message: String,
    },
    /// Call never ran.
    Cancelled,
    /// Execution state unknown.
    Interrupted,
}

/// Open turn observed by the seal protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendOpenTurn {
    /// Owning operation.
    pub operation_id: String,
    /// Open turn.
    pub turn_id: String,
    /// Newest unfilled batch, if any.
    pub unfilled_batch: Option<FrontendUnfilledBatch>,
}

/// Suffix of a tool batch still missing results.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendUnfilledBatch {
    /// Batch id.
    pub tool_batch_id: String,
    /// Missing call ids in source order.
    pub unfilled_call_ids: Vec<String>,
}

/// One queued operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedOperationSummary {
    /// Operation id.
    pub operation_id: String,
    /// Session that owns the queued operation.
    pub session_id: String,
}

/// Availability as a frontend DTO.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontendAvailability {
    /// No active operation or maintenance.
    Idle,
    /// Driving `operation_id`.
    Busy {
        /// Active operation.
        operation_id: String,
    },
    /// Running compaction.
    Compacting {
        /// Session being compacted.
        session_id: String,
    },
}

/// Maintenance projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendMaintenance {
    /// Maintenance id.
    pub id: String,
    /// Phase.
    pub phase: FrontendMaintenancePhase,
    /// Optional progress or terminal detail.
    pub message: Option<String>,
}

/// Maintenance phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrontendMaintenancePhase {
    /// Admitted.
    Accepted,
    /// Driver started.
    Started,
    /// Progress update.
    Progress,
    /// Terminal success.
    Settled,
    /// Terminal failure.
    Failed,
    /// Cancelled.
    Cancelled,
}

/// Latest token usage observation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrontendTokenUsage {
    /// Input tokens.
    pub input_tokens: Option<u64>,
    /// Output tokens.
    pub output_tokens: Option<u64>,
    /// Cache-read tokens.
    pub cache_read_tokens: Option<u64>,
    /// Cache-write tokens.
    pub cache_write_tokens: Option<u64>,
    /// Reasoning tokens.
    pub reasoning_tokens: Option<u64>,
}

/// One pending confirmation as shown in a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingConfirmationView {
    /// Confirmation id.
    pub confirmation_id: u64,
    /// Title.
    pub title: String,
    /// Body.
    pub body: String,
    /// Optional owning operation.
    pub operation_id: Option<String>,
}

/// Secret-free configuration entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendConfigEntry {
    /// Key.
    pub key: String,
    /// Value. Never a secret.
    pub value: String,
    /// Source layer.
    pub source: String,
}

/// `/status` payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendStatus {
    /// Availability.
    pub availability: FrontendAvailability,
    /// Queue depth.
    pub queued: usize,
    /// Current generation.
    pub generation: FrontendGeneration,
    /// Tool lineup.
    pub tools: Vec<FrontendToolListing>,
}

/// One tool in `/status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendToolListing {
    /// Tool name.
    pub name: String,
    /// Effect class label.
    pub effect_class: String,
}

/// Service health.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceHealth {
    /// Operating normally.
    Ok,
    /// Degraded (lag, fault) but still serving.
    Degraded {
        /// Stable diagnostic text.
        message: String,
    },
    /// Runtime epoch ended. New work will fail until a new epoch is started.
    RuntimeEpochEnded {
        /// Stable diagnostic text.
        message: String,
    },
    /// Shutdown is in progress.
    ShuttingDown,
}

/// Mapped live agent event. Not stored as a second log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontendOperationEvent {
    /// Operation entered the FIFO.
    OperationQueued {
        /// Operation id.
        operation_id: String,
    },
    /// Operation started driving.
    OperationStarted {
        /// Operation id.
        operation_id: String,
    },
    /// Turn started.
    TurnStarted {
        /// Turn id.
        turn_id: String,
    },
    /// Model call started.
    ModelCallStarted {
        /// Model call id.
        model_call_id: String,
    },
    /// Provider response metadata.
    ModelResponseStarted {
        /// Model call id.
        model_call_id: String,
        /// Provider model name.
        response_model: Option<String>,
        /// Provider response id.
        response_id: Option<String>,
    },
    /// Assistant text increment.
    TextDelta {
        /// Delta text.
        delta: String,
    },
    /// Reasoning increment.
    ReasoningDelta {
        /// Model call id.
        model_call_id: String,
        /// Delta text.
        text: String,
    },
    /// Token usage observation.
    ModelUsageUpdated {
        /// Model call id.
        model_call_id: String,
        /// Latest usage.
        usage: FrontendTokenUsage,
    },
    /// Tool batch requested.
    ToolBatchRequested {
        /// Batch id.
        tool_batch_id: String,
        /// Call count.
        call_count: usize,
    },
    /// Tool execution started.
    ToolExecutionStarted {
        /// Batch id.
        tool_batch_id: String,
        /// Call id.
        tool_call_id: String,
        /// Source index.
        index: usize,
        /// Tool name.
        tool_name: String,
        /// Raw arguments.
        arguments: String,
    },
    /// Latest tool progress tail.
    ToolExecutionProgress {
        /// Batch id.
        tool_batch_id: String,
        /// Call id.
        tool_call_id: String,
        /// Source index.
        index: usize,
        /// Latest tail.
        tail: String,
    },
    /// Tool execution completed.
    ToolExecutionCompleted {
        /// Batch id.
        tool_batch_id: String,
        /// Call id.
        tool_call_id: String,
        /// Source index.
        index: usize,
        /// Tool name.
        tool_name: String,
        /// Model-channel result.
        result: FrontendToolResult,
        /// Transient display detail.
        display: Option<FrontendToolDisplay>,
    },
    /// Final assistant message assembled.
    AssistantMessageCompleted {
        /// Turn id.
        turn_id: String,
        /// Final text.
        content: String,
    },
    /// Turn failed.
    TurnFailed {
        /// Turn id.
        turn_id: String,
        /// Failure kind label.
        kind: String,
        /// Message.
        message: String,
    },
    /// A prior unfinished turn was sealed.
    PriorTurnSealed {
        /// Sealed turn id.
        turn_id: String,
    },
    /// Automatic compaction started.
    ContextCompactionStarted,
    /// Automatic compaction committed.
    ContextCompactionCompleted {
        /// Opaque boundary.
        covers_up_to: String,
    },
    /// Automatic compaction failed; turn continues.
    ContextCompactionFailed {
        /// Message.
        message: String,
    },
    /// Cancel was accepted.
    CancellationRequested {
        /// Operation id.
        operation_id: String,
        /// Reason label.
        reason: String,
    },
    /// A model call failed with a recoverable fault and will be retried
    /// after the delay. The failed attempt's streamed deltas are discarded.
    ModelRetryScheduled {
        /// Model call id.
        model_call_id: String,
        /// Retry ordinal about to run (1-based).
        attempt: u32,
        /// Configured additional attempts per model call.
        max_retries: u32,
        /// Backoff before the retry, milliseconds.
        delay_ms: u64,
        /// Bounded diagnostic summary of why the attempt failed.
        reason: String,
    },
    /// Turn cancelled durably.
    TurnCancelled {
        /// Turn id.
        turn_id: String,
        /// Reason label.
        reason: String,
    },
    /// Operation settled.
    OperationSettled {
        /// Operation id.
        operation_id: String,
        /// Session that owns this operation.
        session_id: String,
        /// Status label.
        status: String,
        /// Durability label.
        durability: String,
        /// Durable Session revision when this settlement committed.
        session_revision: philo_agent_runtime::SettlementRevision,
    },
}

/// Model-channel tool result DTO.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontendToolResult {
    /// Success.
    Success {
        /// Content.
        content: String,
    },
    /// Business error.
    Error {
        /// Code.
        code: String,
        /// Message.
        message: String,
    },
}

/// Transient tool display DTO.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendToolDisplay {
    /// Human-readable detail.
    pub detail: String,
    /// Ordered facts.
    pub facts: Vec<(String, String)>,
}
