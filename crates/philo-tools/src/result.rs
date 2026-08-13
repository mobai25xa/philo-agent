/// A normal tool response that can be shown to a subsequent model call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolResult {
    Success { content: String },
    Error { code: String, message: String },
}

impl ToolResult {
    pub fn success(content: impl Into<String>) -> Self {
        Self::Success {
            content: content.into(),
        }
    }
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Error {
            code: code.into(),
            message: message.into(),
        }
    }
    pub fn as_error(&self) -> Option<ToolResultError> {
        match self {
            Self::Error { code, message } => Some(ToolResultError::new(code, message)),
            _ => None,
        }
    }
    pub fn content(&self) -> Option<&str> {
        match self {
            Self::Success { content } => Some(content),
            _ => None,
        }
    }
}

/// Stable, model-visible error from a tool invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolResultError {
    code: String,
    message: String,
}

impl ToolResultError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
    pub fn code(&self) -> &str {
        &self.code
    }
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// One ordered display fact: a stable name and its string value.
///
/// Fact-name vocabularies (`exit_code`, `duration_ms`, `bytes_total`, ...)
/// belong to the individual tools and are tuned during real use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolFact {
    name: String,
    value: String,
}

impl ToolFact {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// Transient display-channel payload: full human-readable detail plus
/// ordered key-value facts. Text only; never persisted, never part of any
/// model request — it exists solely for event-driven presentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDisplay {
    detail: String,
    facts: Vec<ToolFact>,
}

impl ToolDisplay {
    /// Creates a display payload from its human-readable detail text.
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            facts: Vec::new(),
        }
    }
    /// Appends one ordered key-value fact.
    pub fn with_fact(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.facts.push(ToolFact::new(name, value));
        self
    }
    pub fn detail(&self) -> &str {
        &self.detail
    }
    pub fn facts(&self) -> &[ToolFact] {
        &self.facts
    }
}

/// Dual-channel tool outcome (M10).
///
/// The model channel (`result`) is the durable fact: it enters the Session,
/// replays into later model calls, and is truncated (if at all) inside the
/// tool handler before construction — nothing above the tool boundary may
/// rewrite it. The display channel (`display`) is transient detail that only
/// travels on events. Both success and error results may carry a display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RichToolResult {
    result: ToolResult,
    display: Option<ToolDisplay>,
}

impl RichToolResult {
    /// Wraps a model-channel result without display detail.
    pub fn new(result: ToolResult) -> Self {
        Self {
            result,
            display: None,
        }
    }
    /// Convenience: a success result without display detail.
    pub fn success(content: impl Into<String>) -> Self {
        Self::new(ToolResult::success(content))
    }
    /// Convenience: a business-error result without display detail.
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(ToolResult::error(code, message))
    }
    /// Attaches the transient display payload.
    pub fn with_display(mut self, display: ToolDisplay) -> Self {
        self.display = Some(display);
        self
    }
    /// The durable model-channel result.
    pub fn result(&self) -> &ToolResult {
        &self.result
    }
    /// The transient display payload, if any.
    pub fn display(&self) -> Option<&ToolDisplay> {
        self.display.as_ref()
    }
    /// Splits this outcome into its two channels.
    pub fn into_parts(self) -> (ToolResult, Option<ToolDisplay>) {
        (self.result, self.display)
    }
}
