//! CLI-level failures that occur before or while dispatching a command.

/// Invalid command input or startup configuration. The composition root
/// renders this to stderr and exits with code 2.
#[derive(Debug, PartialEq, Eq)]
pub struct UsageError(pub String);

impl UsageError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    /// Appends where the offending value came from.
    pub fn at(self, origin: &str) -> Self {
        Self(format!("{} (from {origin})", self.0))
    }
}
