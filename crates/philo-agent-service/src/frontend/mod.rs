//! Frontend protocol: commands, updates, snapshots, and the client handle.

pub(crate) mod client;
pub(crate) mod command;
pub(crate) mod feed;
pub(crate) mod snapshot;
pub(crate) mod update;

pub use client::FrontendClient;
pub use command::{
    ConfirmationDecision, DetachReason, FrontendAttachment, FrontendCommand,
    FrontendReasoningEffort,
};
pub use feed::ResyncRequired;
pub use snapshot::{
    DurableSessionView, FrontendAssistantBlock, FrontendAvailability, FrontendConfigEntry,
    FrontendContextMessage, FrontendGeneration, FrontendMaintenance, FrontendMaintenancePhase,
    FrontendOpenTurn, FrontendOperationEvent, FrontendSnapshot, FrontendStatus, FrontendTokenUsage,
    FrontendToolDisplay, FrontendToolListing, FrontendToolResult, FrontendToolResultOutcome,
    FrontendUnfilledBatch, FrontendUserPart, PendingConfirmationView, QueuedOperationSummary,
    ServiceHealth,
};
pub use update::{FrontendUpdate, FrontendUpdateKind};

pub(crate) use client::CommandEnvelope;
pub(crate) use feed::FrontendFeed;
