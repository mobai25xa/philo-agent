//! Service-level errors. Channel backpressure is a result, not an error.

use std::fmt;

/// Failure starting or running the application service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceError {
    /// The runtime handle or subscription is no longer usable.
    RuntimeUnavailable {
        /// Stable diagnostic text.
        message: String,
    },
    /// A required bounded lane closed unexpectedly.
    Disconnected {
        /// Which lane closed.
        lane: &'static str,
    },
}

impl ServiceError {
    /// Runtime-side failure with stable diagnostic text.
    pub fn runtime(message: impl Into<String>) -> Self {
        Self::RuntimeUnavailable {
            message: message.into(),
        }
    }
}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RuntimeUnavailable { message } => {
                write!(f, "runtime unavailable: {message}")
            }
            Self::Disconnected { lane } => write!(f, "{lane} disconnected"),
        }
    }
}

impl std::error::Error for ServiceError {}

/// Result of [`crate::FrontendClient::try_command`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandSubmitResult {
    /// The command is on a service lane and will be correlated by this id.
    Accepted(crate::FrontendRequestId),
    /// The target bounded lane is full. The command was not queued.
    Backpressured,
    /// The service actor is gone.
    Disconnected,
}

/// Result of [`crate::FrontendClient::recv_until`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecvOutcome {
    /// One frontend update.
    Update(crate::FrontendUpdate),
    /// The deadline elapsed with no update.
    Timeout,
    /// The service actor is gone or the feed was dropped.
    Disconnected,
}
