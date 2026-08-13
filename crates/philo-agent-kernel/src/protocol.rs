use std::fmt;

macro_rules! string_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        pub struct $name(pub(crate) String);
        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

string_id!(TurnId, "Identifies a turn.");
string_id!(
    ModelCallId,
    "Identifies a logical model call within a turn."
);
string_id!(EffectId, "Identifies an outstanding external effect.");
string_id!(
    ToolBatchId,
    "Identifies one tool batch round within a turn."
);
string_id!(ToolCallId, "Identifies a model-originated tool call.");

/// One part of a multi-part user message. Image bytes are opaque to the
/// kernel: never parsed, never validated, no I/O performed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserPart {
    Text(String),
    Image { media_type: String, bytes: Vec<u8> },
}

/// Why constructing a [`UserMessage`] was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidUserMessage {
    EmptyParts,
    EmptyTextPart,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserMessage {
    parts: Vec<UserPart>,
}
impl UserMessage {
    /// Plain-text convenience constructor.
    ///
    /// # Panics
    ///
    /// Panics when `text` is empty; use [`UserMessage::from_parts`] for
    /// fallible construction.
    pub fn new(text: impl Into<String>) -> Self {
        Self::from_parts(vec![UserPart::Text(text.into())])
            .expect("plain-text user message must not be empty")
    }
    /// Multi-part constructor: parts must be non-empty and text parts must
    /// not be empty strings. Image-only messages are valid.
    pub fn from_parts(parts: Vec<UserPart>) -> Result<Self, InvalidUserMessage> {
        if parts.is_empty() {
            return Err(InvalidUserMessage::EmptyParts);
        }
        for part in &parts {
            if matches!(part, UserPart::Text(text) if text.is_empty()) {
                return Err(InvalidUserMessage::EmptyTextPart);
            }
        }
        Ok(Self { parts })
    }
    pub fn parts(&self) -> &[UserPart] {
        &self.parts
    }
}

/// One complete model-originated tool call. Raw arguments are preserved exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelToolCall {
    id: ToolCallId,
    name: String,
    arguments: String,
}
impl KernelToolCall {
    pub fn new(id: ToolCallId, name: impl Into<String>, arguments: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            arguments: arguments.into(),
        }
    }
    pub fn id(&self) -> &ToolCallId {
        &self.id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn arguments(&self) -> &str {
        &self.arguments
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KernelToolResultOutcome {
    Success { content: String },
    Error { code: String, message: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelToolResult {
    call_id: ToolCallId,
    outcome: KernelToolResultOutcome,
}
impl KernelToolResult {
    pub fn success(call_id: ToolCallId, content: impl Into<String>) -> Self {
        Self {
            call_id,
            outcome: KernelToolResultOutcome::Success {
                content: content.into(),
            },
        }
    }
    pub fn error(call_id: ToolCallId, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            call_id,
            outcome: KernelToolResultOutcome::Error {
                code: code.into(),
                message: message.into(),
            },
        }
    }
    pub fn call_id(&self) -> &ToolCallId {
        &self.call_id
    }
    pub fn outcome(&self) -> &KernelToolResultOutcome {
        &self.outcome
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AssistantOutputKind {
    FinalText,
    ToolCalls(Vec<KernelToolCall>),
    Unsupported,
}

/// Complete semantic assistant output supplied after a model stream completes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssistantOutput {
    text: String,
    kind: AssistantOutputKind,
}
impl AssistantOutput {
    pub fn final_text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: AssistantOutputKind::FinalText,
        }
    }
    pub fn tool_calls(calls: Vec<KernelToolCall>) -> Self {
        Self {
            text: String::new(),
            kind: AssistantOutputKind::ToolCalls(calls),
        }
    }
    pub fn mixed(text: impl Into<String>, calls: Vec<KernelToolCall>) -> Self {
        Self {
            text: text.into(),
            kind: AssistantOutputKind::ToolCalls(calls),
        }
    }
    pub fn with_unsupported_tool_call(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: AssistantOutputKind::Unsupported,
        }
    }
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn tool_call_batch(&self) -> Option<&[KernelToolCall]> {
        match &self.kind {
            AssistantOutputKind::ToolCalls(calls) => Some(calls),
            _ => None,
        }
    }
    pub fn contains_tool_call(&self) -> bool {
        !matches!(self.kind, AssistantOutputKind::FinalText)
    }
    pub(crate) fn is_unsupported(&self) -> bool {
        matches!(self.kind, AssistantOutputKind::Unsupported)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnMessage {
    User(UserMessage),
    Assistant(AssistantOutput),
    AssistantToolCalls {
        tool_batch_id: ToolBatchId,
        calls: Vec<KernelToolCall>,
    },
    ToolResult(KernelToolResult),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnFailure {
    ModelCallFailed { message: String },
    InvalidModelOutput { message: String },
    ToolExecutionFailed { message: String },
    PersistenceFailed { message: String },
    RuntimeDriverFailed { message: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KernelPhaseView {
    ExpectingTurnStart,
    ExpectingModelCompletion {
        effect_id: EffectId,
        model_call_id: ModelCallId,
    },
    ExpectingToolBatchCompletion {
        effect_id: EffectId,
        tool_batch_id: ToolBatchId,
    },
    Terminated {
        outcome: TurnOutcome,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KernelInput {
    BeginTurn {
        turn_id: TurnId,
        user_message: UserMessage,
        /// Upper bound of tool rounds frozen for this turn; `0` never exposes tools.
        max_tool_rounds: u32,
    },
    ModelCallCompleted {
        effect_id: EffectId,
        output: AssistantOutput,
    },
    ToolBatchCompleted {
        effect_id: EffectId,
        results: Vec<KernelToolResult>,
    },
    TerminationRequested {
        effect_id: EffectId,
        failure: TurnFailure,
    },
    /// User-requested orderly termination of the outstanding effect.
    /// Accepted only when `effect_id` matches, mirroring `TerminationRequested`.
    CancelRequested { effect_id: EffectId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KernelEffect {
    RequestModel {
        effect_id: EffectId,
        model_call_id: ModelCallId,
        turn_messages: Vec<TurnMessage>,
        /// Whether this call may expose the turn's frozen tool definitions.
        tools_allowed: bool,
    },
    ExecuteToolBatch {
        effect_id: EffectId,
        tool_batch_id: ToolBatchId,
        calls: Vec<KernelToolCall>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KernelObservation {
    TurnBegan {
        turn_id: TurnId,
        user_message: UserMessage,
    },
    ModelCallRequested {
        model_call_id: ModelCallId,
        effect_id: EffectId,
    },
    AssistantOutputAccepted {
        model_call_id: ModelCallId,
        output: AssistantOutput,
    },
    AssistantToolCallsAccepted {
        model_call_id: ModelCallId,
        tool_batch_id: ToolBatchId,
        calls: Vec<KernelToolCall>,
    },
    ToolBatchRequested {
        tool_batch_id: ToolBatchId,
        effect_id: EffectId,
    },
    ToolResultsAccepted {
        tool_batch_id: ToolBatchId,
        results: Vec<KernelToolResult>,
    },
    TurnFailureAccepted {
        effect_id: EffectId,
        failure: TurnFailure,
    },
    TurnTerminated {
        outcome: TurnOutcome,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurabilityRequirement {
    BeforeNextEffect,
    BeforeSettlement,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelTransition {
    pub next_state: crate::state::KernelState,
    pub phase: KernelPhaseView,
    pub observations: Vec<KernelObservation>,
    pub durability: DurabilityRequirement,
    pub effect: Option<KernelEffect>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KernelInputRejectionReason {
    InputNotAccepted,
    EffectIdMismatch {
        expected: EffectId,
        received: EffectId,
    },
    EffectAlreadyCompleted {
        effect_id: EffectId,
    },
    UnsupportedAssistantOutput,
    InvalidToolCalls,
    ToolResultsMismatch,
    KernelTerminated,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelInputRejection {
    pub(crate) phase: KernelPhaseView,
    pub(crate) reason: KernelInputRejectionReason,
}
impl KernelInputRejection {
    pub fn phase(&self) -> &KernelPhaseView {
        &self.phase
    }
    pub fn reason(&self) -> &KernelInputRejectionReason {
        &self.reason
    }
}
