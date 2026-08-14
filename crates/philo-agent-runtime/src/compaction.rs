//! Public result and error vocabulary for manual context compaction.

use crate::AgentAvailability;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompactionReport {
    /// One durable compaction entry was committed at this opaque boundary.
    Compacted { covers_up_to: String },
    /// The session has no boundary that can advance while retaining the
    /// configured recent-turn tail. The model was not called.
    NothingToCompact,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompactionError {
    /// The runtime was not idle when maintenance admission was attempted.
    Unavailable {
        availability: AgentAvailability,
    },
    Session {
        message: String,
    },
    Model {
        message: String,
    },
    InvalidModelOutput {
        message: String,
    },
}

impl CompactionError {
    pub fn message(&self) -> &str {
        match self {
            Self::Unavailable { .. } => "runtime is not available for compaction",
            Self::Session { message }
            | Self::Model { message }
            | Self::InvalidModelOutput { message } => message,
        }
    }
}
