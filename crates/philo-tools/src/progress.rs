use std::sync::Arc;

/// Best-effort display-channel live sink injected into each `invoke`.
///
/// Pushing never fails and must not block the tool for long. The sink does
/// not change the durable `RichToolResult` and is ignored by handlers that
/// do not stream.
#[derive(Clone)]
pub struct ToolProgressSink {
    push: Arc<dyn Fn(&str) + Send + Sync>,
}

impl ToolProgressSink {
    /// A sink that discards every push. Missing or ignored progress keeps
    /// the pre-L1 one-shot result behavior.
    pub fn noop() -> Self {
        Self {
            push: Arc::new(|_| {}),
        }
    }

    /// Builds a sink from a caller-owned callback (Runtime coalescer).
    pub fn from_fn<F>(push: F) -> Self
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        Self {
            push: Arc::new(push),
        }
    }

    /// Appends display text. Never returns an error.
    pub fn push_text(&self, text: &str) {
        if !text.is_empty() {
            (self.push)(text);
        }
    }
}

impl Default for ToolProgressSink {
    fn default() -> Self {
        Self::noop()
    }
}

impl std::fmt::Debug for ToolProgressSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ToolProgressSink")
    }
}

impl PartialEq for ToolProgressSink {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for ToolProgressSink {}
