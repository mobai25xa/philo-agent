use std::future::Future;
use std::pin::Pin;

use crate::{ModelCallSnapshot, ToolCallId, UserPart};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelToolCall {
    pub tool_call_id: ToolCallId,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelToolResultOutcome {
    Success {
        content: String,
    },
    Error {
        code: String,
        message: String,
    },
    /// The call never executed because its turn was cancelled. Rendering
    /// into provider-readable text is the adapter's responsibility.
    Cancelled,
    /// The process was interrupted while the call was outstanding: whether
    /// it executed is unknown (M11). Mirrors the durable
    /// `ToolResultOutcome::Interrupted` and the runtime's synthesized
    /// placeholder for dangling batches of terminated turns. Rendering is
    /// the adapter's responsibility and must tell the model to verify
    /// actual state before assuming.
    Interrupted,
}

/// One ordered block of a completed model call. Text and tool calls may
/// coexist and interleave; empty Text is invalid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelAssistantBlock {
    Text { text: String },
    ToolCall(ModelToolCall),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelMessage {
    System {
        content: String,
    },
    /// Durable summary of an earlier conversation prefix. Adapters map this
    /// to their provider-neutral instructions channel rather than a user
    /// message.
    Summary {
        text: String,
    },
    /// The user turn's full multi-part payload, replayed verbatim.
    User {
        parts: Vec<UserPart>,
    },
    /// One assistant turn: a tool round or a final message, never split.
    Assistant {
        blocks: Vec<ModelAssistantBlock>,
    },
    ToolResult {
        tool_call_id: ToolCallId,
        outcome: ModelToolResultOutcome,
    },
}

/// One normalized fragment of a model tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: String,
}
impl ToolCallDelta {
    pub fn new(
        index: usize,
        id: Option<impl Into<String>>,
        name: Option<impl Into<String>>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            index,
            id: id.map(Into::into),
            name: name.map(Into::into),
            arguments: arguments.into(),
        }
    }
}

/// Token accounting observed for one logical model call. All fields are
/// optional; providers report what they know. Runtime-owned value type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

impl TokenUsage {
    /// Derives input plus output tokens when both are known.
    pub fn total_tokens(&self) -> Option<u64> {
        self.input_tokens?.checked_add(self.output_tokens?)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelEvent {
    /// Optional response metadata observed when the provider stream opens.
    /// At most one per logical call; absent streams keep the 0.4 behavior.
    ResponseStarted {
        response_model: Option<String>,
        response_id: Option<String>,
    },
    TextDelta(String),
    /// Visible reasoning text increment: a transient fact that never joins
    /// the assembled AssistantOutput and is never written to the Session.
    /// Absent streams keep the pre-M7 behavior.
    ReasoningDelta {
        text: String,
    },
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    /// Token-usage observation; may occur multiple times per logical call
    /// (incremental provider updates), the last one wins. Transient fact.
    UsageUpdated {
        usage: TokenUsage,
    },
    /// Authoritative assistant output for this logical call, in SDK item
    /// order. Text and tool calls may coexist; deltas are not the source
    /// of truth.
    Completed {
        blocks: Vec<ModelAssistantBlock>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelError {
    message: String,
}
impl ModelError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
    pub fn message(&self) -> &str {
        &self.message
    }
}

pub type RuntimeFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Event stream of one logical model call.
///
/// Dropping the stream before it completes is the cancellation signal: the
/// implementation must terminate the underlying call, release its connection
/// and resources, and produce no further observable side effects. Drop is
/// not required to distinguish cancellation from an abnormal consumer exit.
pub trait ModelEventStream: Send {
    fn next<'a>(&'a mut self) -> RuntimeFuture<'a, Option<Result<ModelEvent, ModelError>>>;
}
pub trait ModelPort: Send + Sync {
    fn start<'a>(
        &'a self,
        request: ModelCallSnapshot,
    ) -> RuntimeFuture<'a, Result<Box<dyn ModelEventStream>, ModelError>>;
}
