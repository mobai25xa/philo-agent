use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::{RichToolResult, ToolArguments, ToolDefinition, ToolInvocation, ToolProgressSink};

/// Infrastructure failure that prevented a normal tool result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolPortError {
    message: String,
}
impl ToolPortError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
    pub fn message(&self) -> &str {
        &self.message
    }
}

pub type ToolFuture<'a> =
    Pin<Box<dyn Future<Output = Result<RichToolResult, ToolPortError>> + Send + 'a>>;
/// Future returned by a domain handler.
pub type ToolHandlerFuture<'a> = Pin<Box<dyn Future<Output = RichToolResult> + Send + 'a>>;

/// Object-safe async business handler. The registry validates arguments
/// first. Handlers produce the dual-channel outcome directly (M10): business
/// errors travel as the model channel's `Error` variant (optionally with
/// display detail); infrastructure failures stay a Registry/Port concern.
pub trait ToolHandler: Send + Sync {
    fn call<'a>(&'a self, arguments: ToolArguments) -> ToolHandlerFuture<'a>;

    /// Default: ignore the sink and produce the same one-shot result as
    /// [`ToolHandler::call`]. Streaming tools override this method.
    fn call_with_progress<'a>(
        &'a self,
        arguments: ToolArguments,
        progress: ToolProgressSink,
    ) -> ToolHandlerFuture<'a> {
        let _ = progress;
        self.call(arguments)
    }
}

impl<F, Fut> ToolHandler for F
where
    F: Fn(ToolArguments) -> Fut + Send + Sync,
    Fut: Future<Output = RichToolResult> + Send + 'static,
{
    fn call<'a>(&'a self, arguments: ToolArguments) -> ToolHandlerFuture<'a> {
        Box::pin((self)(arguments))
    }
}

/// Port used by Runtime; it has no persistence or kernel responsibilities.
pub trait ToolPort: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn invoke<'a>(
        &'a self,
        invocation: ToolInvocation,
        progress: ToolProgressSink,
    ) -> ToolFuture<'a>;
}

pub(crate) type Handler = Arc<dyn ToolHandler>;
