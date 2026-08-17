use std::collections::VecDeque;
use std::sync::Mutex;

use philo_agent_runtime::{
    RichToolResult, ToolDefinition, ToolDisplay, ToolFuture, ToolInvocation, ToolPort,
    ToolPortError, ToolProgressSink,
};

use super::gate::Gate;

#[derive(Clone, Debug)]
pub enum FakeToolResult {
    Success(String),
    /// Success carrying a display-channel payload (M10 dual channel).
    SuccessWithDisplay {
        content: String,
        display: ToolDisplay,
    },
    BusinessError {
        code: String,
        message: String,
    },
    InfrastructureError(String),
    /// Blocks the executing call until the gate opens, then succeeds.
    /// Creates a deterministic mid-batch cancellation window.
    GatedSuccess {
        gate: Gate,
        content: String,
    },
    /// Pushes each chunk through the progress sink, then succeeds.
    StreamingSuccess {
        chunks: Vec<String>,
        content: String,
    },
}

impl FakeToolResult {
    pub fn success(content: impl Into<String>) -> Self {
        Self::Success(content.into())
    }

    pub fn success_with_display(content: impl Into<String>, display: ToolDisplay) -> Self {
        Self::SuccessWithDisplay {
            content: content.into(),
            display,
        }
    }

    pub fn business_error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::BusinessError {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn infrastructure_error(message: impl Into<String>) -> Self {
        Self::InfrastructureError(message.into())
    }

    pub fn gated_success(gate: &Gate, content: impl Into<String>) -> Self {
        Self::GatedSuccess {
            gate: gate.clone(),
            content: content.into(),
        }
    }

    pub fn streaming_success(
        chunks: impl IntoIterator<Item = impl Into<String>>,
        content: impl Into<String>,
    ) -> Self {
        Self::StreamingSuccess {
            chunks: chunks.into_iter().map(Into::into).collect(),
            content: content.into(),
        }
    }
}

/// A test-only ToolPort that captures every invocation before replaying results.
pub struct FakeTool {
    definitions: Vec<ToolDefinition>,
    results: Mutex<VecDeque<FakeToolResult>>,
    invocations: Mutex<Vec<ToolInvocation>>,
}

impl FakeTool {
    pub fn new(
        definitions: impl IntoIterator<Item = ToolDefinition>,
        results: impl IntoIterator<Item = FakeToolResult>,
    ) -> Self {
        Self {
            definitions: definitions.into_iter().collect(),
            results: Mutex::new(results.into_iter().collect()),
            invocations: Mutex::new(Vec::new()),
        }
    }

    pub fn one(definition: ToolDefinition, result: FakeToolResult) -> Self {
        Self::new([definition], [result])
    }

    pub fn definitions_snapshot(&self) -> Vec<ToolDefinition> {
        self.definitions.clone()
    }

    pub fn invocations(&self) -> Vec<ToolInvocation> {
        self.invocations
            .lock()
            .expect("fake tool invocations mutex")
            .clone()
    }

    pub fn invocation_count(&self) -> usize {
        self.invocations
            .lock()
            .expect("fake tool invocations mutex")
            .len()
    }
}

impl ToolPort for FakeTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.definitions_snapshot()
    }

    fn invoke<'a>(
        &'a self,
        invocation: ToolInvocation,
        progress: ToolProgressSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            self.invocations
                .lock()
                .expect("fake tool invocations mutex")
                .push(invocation);
            let result = self
                .results
                .lock()
                .expect("fake tool results mutex")
                .pop_front()
                .expect("fake tool invoked more times than scripted");
            match result {
                FakeToolResult::Success(content) => Ok(RichToolResult::success(content)),
                FakeToolResult::SuccessWithDisplay { content, display } => {
                    Ok(RichToolResult::success(content).with_display(display))
                }
                FakeToolResult::BusinessError { code, message } => {
                    Ok(RichToolResult::error(code, message))
                }
                FakeToolResult::InfrastructureError(message) => Err(ToolPortError::new(message)),
                FakeToolResult::GatedSuccess { gate, content } => {
                    gate.wait().await;
                    Ok(RichToolResult::success(content))
                }
                FakeToolResult::StreamingSuccess { chunks, content } => {
                    for chunk in chunks {
                        progress.push_text(&chunk);
                    }
                    Ok(RichToolResult::success(content))
                }
            }
        })
    }
}
