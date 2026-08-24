use crate::{
    AgentFailure, AssistantMessage, CancelReason, ModelCallId, OperationId, OperationStatus,
    SettlementDurability, TokenUsage, ToolBatchId, ToolCallId, TurnId,
};

/// Published notification stream of one operation.
///
/// `AgentEvent` is a non-exhaustive notification enum: new milestones may add
/// events, and consumers outside this crate must tolerate unknown variants
/// with a wildcard match arm. Decision-protocol enums (Kernel inputs,
/// `OperationPhase`, Session entry kinds) keep their exhaustive stance.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentEvent {
    /// Transient: the operation entered the FIFO follow-up queue. Published
    /// at enqueue time, never written to the Session.
    OperationQueued {
        operation_id: OperationId,
    },
    /// Published when the operation actually starts driving (dequeue time
    /// for queued operations), not when it is accepted.
    OperationStarted {
        operation_id: OperationId,
    },
    TurnStarted {
        turn_id: TurnId,
    },
    ModelCallStarted {
        model_call_id: ModelCallId,
    },
    /// Transient response-metadata observation; never written to the Session.
    /// Published after the matching `ModelCallStarted`, before the call's
    /// output is assembled, and at most once per logical model call.
    ModelResponseStarted {
        model_call_id: ModelCallId,
        response_model: Option<String>,
        response_id: Option<String>,
    },
    TextDelta {
        delta: String,
    },
    /// Transient visible-reasoning increment; never written to the Session.
    /// Published between the matching `ModelCallStarted` and that call's
    /// assembled completion. Absent streams keep the pre-M7 behavior.
    ReasoningDelta {
        model_call_id: ModelCallId,
        text: String,
    },
    /// Transient token-usage observation; never written to the Session.
    /// May occur multiple times per logical call, the last one wins.
    ModelUsageUpdated {
        model_call_id: ModelCallId,
        usage: TokenUsage,
    },
    ToolBatchRequested {
        tool_batch_id: ToolBatchId,
        call_count: usize,
    },
    /// Published before the ToolPort is invoked; carries the tool identity
    /// and the model's raw argument text (M10 display-channel payload).
    ToolExecutionStarted {
        tool_batch_id: ToolBatchId,
        tool_call_id: ToolCallId,
        index: usize,
        tool_name: String,
        arguments: String,
    },
    /// Transient live tail for one in-flight tool call. Never written to
    /// the Session. Published after the matching `Started` and before that
    /// call's `Completed`. Same-call unconsumed events are replaced.
    ToolExecutionProgress {
        tool_batch_id: ToolBatchId,
        tool_call_id: ToolCallId,
        index: usize,
        tail: String,
    },
    /// Published only after the results barrier (or cancellation
    /// transaction) committed. `result` is same-source-same-value with the
    /// durable `SessionToolResult`; `display` is the transient display
    /// channel's terminal outlet. Both reuse the `philo-tools` types directly.
    ToolExecutionCompleted {
        tool_batch_id: ToolBatchId,
        tool_call_id: ToolCallId,
        index: usize,
        tool_name: String,
        result: philo_tools::ToolResult,
        display: Option<philo_tools::ToolDisplay>,
    },
    AssistantMessageCompleted {
        turn_id: TurnId,
        message: AssistantMessage,
    },
    TurnFailed {
        turn_id: TurnId,
        failure: AgentFailure,
    },
    /// Transient (M11): a stale unfinished turn left by an interrupted
    /// process was sealed — its terminal facts committed durably — before
    /// this operation's turn started. Published once per seal, after
    /// `OperationStarted` and before `TurnStarted`. Sealing does not go
    /// through the cancellation event channel.
    PriorTurnSealed {
        turn_id: TurnId,
    },
    /// Transient: automatic pre-turn context compaction has started.
    ContextCompactionStarted,
    /// Transient: the compaction entry committed at the opaque boundary.
    ContextCompactionCompleted {
        covers_up_to: String,
    },
    /// Transient warning: automatic compaction failed and the turn will
    /// continue against the uncompressed projection.
    ContextCompactionFailed {
        message: String,
    },
    /// Transient: a cancel request was accepted (invalid cancels publish
    /// nothing). At most once per operation; never written to the Session.
    /// The reason is `User` or `Timeout`; seals never use this channel.
    CancellationRequested {
        operation_id: OperationId,
        reason: CancelReason,
    },
    /// Transient: a model call failed with a recoverable delivery fault and
    /// the engine will re-issue the identical call after `delay_ms`. Never
    /// written to the Session. The failed attempt's streamed deltas are
    /// discarded; consumers should close any open streaming view on receipt.
    ModelRetryScheduled {
        model_call_id: ModelCallId,
        /// Retry ordinal about to run (1-based).
        attempt: u32,
        max_retries: u32,
        delay_ms: u64,
        /// Structured four-question summary of why the attempt failed.
        failure: AgentFailure,
    },
    /// The cancellation terminal facts committed durably; published before
    /// the matching `OperationSettled`. The reason is `User` or `Timeout`.
    TurnCancelled {
        turn_id: TurnId,
        reason: CancelReason,
    },
    OperationSettled {
        operation_id: OperationId,
        status: OperationStatus,
        durability: SettlementDurability,
        /// Durable Session revision when this settlement committed. Never forged.
        session_revision: crate::SettlementRevision,
    },
}
