//! Frontend protocol: commands, updates, snapshots, and the client handle.

pub(crate) mod client;
pub(crate) mod command;
pub(crate) mod feed;
pub(crate) mod lease;
pub(crate) mod snapshot;
pub(crate) mod supervisor;
pub(crate) mod update;

pub use client::FrontendClient;
pub use command::{
    ConfirmationDecision, FrontendAttachment, FrontendCommand, FrontendReasoningEffort,
};
pub use feed::ResyncRequired;
pub use lease::{
    AttachError, DetachError, DetachReport, FrontendLease, FrontendLeaseGeneration,
    SupervisorCommand,
};
pub use snapshot::{
    DurableSessionView, FrontendAssistantBlock, FrontendAvailability, FrontendConfigEntry,
    FrontendContextMessage, FrontendFailure, FrontendGeneration, FrontendMaintenance,
    FrontendMaintenancePhase, FrontendModelListing, FrontendOpenTurn, FrontendOperationEvent,
    FrontendSessionSummary, FrontendSnapshot, FrontendStatus, FrontendTokenUsage,
    FrontendToolDisplay, FrontendToolListing, FrontendToolResult, FrontendToolResultOutcome,
    FrontendUnfilledBatch, FrontendUserPart, PendingConfirmationView, QueuedOperationSummary,
    ServiceHealth,
};
pub use update::{FrontendUpdate, FrontendUpdateKind};

pub(crate) use client::CommandEnvelope;
pub(crate) use client::FrontendLanes;
pub(crate) use feed::{FrontendFeed, ReplyCredits};
