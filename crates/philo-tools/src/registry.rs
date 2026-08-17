use std::sync::Arc;

use crate::invocation::ToolArguments;
use crate::port::{Handler, ToolFuture};
use crate::{
    RichToolResult, ToolDefinition, ToolHandler, ToolInvocation, ToolPort, ToolProgressSink,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryError {
    DuplicateName(String),
    InvalidDefinition(String),
}

pub struct ToolRegistryBuilder {
    entries: Vec<(ToolDefinition, Handler)>,
}
impl ToolRegistryBuilder {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
    pub fn register<H: ToolHandler + 'static>(
        mut self,
        definition: ToolDefinition,
        handler: H,
    ) -> Result<Self, RegistryError> {
        if self
            .entries
            .iter()
            .any(|(d, _)| d.name() == definition.name())
        {
            return Err(RegistryError::DuplicateName(definition.name().to_owned()));
        }
        self.entries.push((definition, Arc::new(handler)));
        Ok(self)
    }
    pub fn build(self) -> ToolRegistry {
        ToolRegistry {
            entries: Arc::new(self.entries),
        }
    }
}
impl Default for ToolRegistryBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct ToolRegistry {
    entries: Arc<Vec<(ToolDefinition, Handler)>>,
}
impl ToolRegistry {
    /// Creates an empty frozen registry.
    pub fn new() -> Self {
        Self::empty()
    }
    pub fn builder() -> ToolRegistryBuilder {
        ToolRegistryBuilder::new()
    }
    pub fn empty() -> Self {
        ToolRegistryBuilder::new().build()
    }
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.entries.iter().map(|(d, _)| d.clone()).collect()
    }
}
impl Default for ToolRegistry {
    fn default() -> Self {
        Self::empty()
    }
}

impl ToolPort for ToolRegistry {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.definitions()
    }
    fn invoke<'a>(
        &'a self,
        invocation: ToolInvocation,
        progress: ToolProgressSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            // Registry-synthesized errors carry no display detail and
            // never touch the progress sink.
            let Some((definition, handler)) = self
                .entries
                .iter()
                .find(|(d, _)| d.name() == invocation.name())
            else {
                return Ok(RichToolResult::error(
                    "unknown_tool",
                    format!("unknown tool: {}", invocation.name()),
                ));
            };
            let args = match ToolArguments::parse(invocation.raw_arguments()) {
                Ok(args) => args,
                Err(message) => return Ok(RichToolResult::error("invalid_arguments", message)),
            };
            if let Err(message) = definition.validate_arguments(args.as_str()) {
                return Ok(RichToolResult::error("invalid_arguments", message));
            }
            Ok(handler.call_with_progress(args, progress).await)
        })
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("definitions", &self.definitions())
            .finish()
    }
}
