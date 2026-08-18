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

/// Result of crossing a bounded command lane. This is enqueue, not command accept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandDispatch<T> {
    /// The command was queued on its lane. The actor has not accepted it yet.
    Enqueued(T),
    /// The target bounded lane is full. The command was not queued.
    Backpressured,
    /// The target lane closed. The command was not queued.
    Disconnected {
        /// Which lane closed.
        lane: &'static str,
    },
}

/// Why the service actor refused a command after it was dequeued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandReject {
    /// Submit targeted the current session, but none is loaded.
    NoCurrentSession,
    /// The service is shutting down or the runtime is gone.
    NotAccepting,
    /// The bounded child-task pool is full.
    ChildCapacity,
    /// The command payload could not be interpreted.
    InvalidInput {
        /// Stable diagnostic text.
        reason: String,
    },
    /// Runtime admission refused the command.
    AdmissionFailed {
        /// Runtime admission diagnostic.
        message: String,
    },
    /// Cancel was not accepted by the runtime.
    CancelRejected {
        /// Runtime cancel diagnostic.
        message: String,
    },
    /// The confirmation id is not pending.
    UnknownConfirmation,
}

impl fmt::Display for CommandReject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCurrentSession => write!(f, "no current session"),
            Self::NotAccepting => write!(f, "runtime is not accepting work"),
            Self::ChildCapacity => write!(f, "service child capacity reached"),
            Self::InvalidInput { reason } => write!(f, "{reason}"),
            Self::AdmissionFailed { message } => write!(f, "{message}"),
            Self::CancelRejected { message } => write!(f, "{message}"),
            Self::UnknownConfirmation => write!(f, "unknown confirmation"),
        }
    }
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
