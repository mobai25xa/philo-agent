//! Service → frontend updates. Every envelope carries epoch + revision.

use crate::error::CommandReject;
use crate::frontend::command::ConfirmationDecision;
use crate::frontend::snapshot::FrontendGeneration;
use crate::frontend::snapshot::{
    DurableSessionView, FrontendAvailability, FrontendConfigEntry, FrontendMaintenance,
    FrontendOperationEvent, FrontendSnapshot, FrontendStatus, ServiceHealth,
};
use crate::ids::{FrontendEpoch, FrontendRequestId, FrontendRevision};

/// One update on the bounded frontend feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendUpdate {
    /// Service/runtime epoch. Discard if older than the frontend's epoch.
    pub epoch: FrontendEpoch,
    /// Monotonic service revision. Discard if it moves backwards.
    pub revision: FrontendRevision,
    /// Command that produced this update, when applicable.
    pub request_id: Option<FrontendRequestId>,
    /// Payload.
    pub kind: FrontendUpdateKind,
}

/// Update payloads. Terminal facts are not reconstructed from this stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontendUpdateKind {
    /// The actor accepted a command that does not admit Runtime work.
    CommandAccepted,
    /// Runtime admitted a submit after `runtime.submit` returned Ok.
    SubmitAccepted {
        /// Admitted operation.
        operation_id: String,
        /// Turn minted at admission.
        turn_id: String,
    },
    /// Runtime admitted compaction after `start_compaction` returned Ok.
    CompactionAccepted {
        /// Admitted maintenance task.
        maintenance_id: String,
    },
    /// The actor refused a dequeued command.
    CommandRejected {
        /// Structured refusal.
        reason: CommandReject,
    },
    /// Runtime admitted an operation.
    OperationAccepted {
        /// Operation id.
        operation_id: String,
        /// Session that owns the operation.
        session_id: String,
        /// Turn id.
        turn_id: String,
    },
    /// Mapped live agent event for the current operation.
    OperationEvent(FrontendOperationEvent),
    /// Availability projection.
    AvailabilityChanged {
        /// Idle / busy / compacting.
        availability: FrontendAvailability,
        /// Queue depth.
        queued: usize,
    },
    /// Maintenance projection changed.
    MaintenanceChanged(FrontendMaintenance),
    /// Current session was replaced.
    SessionLoaded {
        /// Session id.
        session_id: String,
        /// Durable view at load time.
        view: DurableSessionView,
    },
    /// Preview completed. Stale `request_id` / generation are discarded.
    SessionPreviewed {
        /// Session id.
        session_id: String,
        /// Durable view.
        view: DurableSessionView,
    },
    /// Durable session ids, plus the uncommitted current session when needed.
    SessionListLoaded {
        /// Session ids in service-stable order.
        session_ids: Vec<String>,
    },
    /// Current generation was replaced.
    GenerationInstalled {
        /// Secret-free display metadata.
        display: FrontendGeneration,
    },
    /// Generation assembly failed; the previous generation remains current.
    GenerationInstallFailed {
        /// Requested model name.
        name: String,
        /// Stable diagnostic text.
        message: String,
    },
    /// Effective configuration entries.
    ConfigChanged {
        /// Secret-free entries.
        entries: Vec<FrontendConfigEntry>,
    },
    /// A confirmation is waiting.
    ConfirmationRequested {
        /// Confirmation id.
        confirmation_id: u64,
        /// Question shown to the user.
        title: String,
        /// Body shown to the user.
        body: String,
    },
    /// A confirmation was answered or auto-denied.
    ConfirmationResolved {
        /// Confirmation id.
        confirmation_id: u64,
        /// Final decision.
        decision: ConfirmationDecision,
    },
    /// Composed snapshot. Replaces frontend business projection.
    SnapshotReady(Box<FrontendSnapshot>),
    /// Frontend feed overflowed. Request a snapshot at `latest_revision`.
    ResyncRequired {
        /// Service revision at overflow/recovery.
        latest_revision: FrontendRevision,
    },
    /// Service health changed.
    ServiceHealthChanged {
        /// Current health.
        health: ServiceHealth,
    },
    /// Status query result.
    StatusReady(FrontendStatus),
}

impl FrontendUpdate {
    pub(crate) fn new(
        epoch: FrontendEpoch,
        revision: FrontendRevision,
        request_id: Option<FrontendRequestId>,
        kind: FrontendUpdateKind,
    ) -> Self {
        Self {
            epoch,
            revision,
            request_id,
            kind,
        }
    }
}
