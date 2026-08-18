//! Frontend → service commands. Control-lane commands are never mixed with
//! submit/query traffic.

use crate::ids::{FrontendInstanceId, FrontendRevision};

/// Why a frontend instance detached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DetachReason {
    /// User quit or idle Ctrl+C.
    UserExit,
    /// Terminal or frontend fault. Must not cancel Runtime work.
    Fault {
        /// Stable diagnostic text.
        message: String,
    },
    /// Supervisor is restarting the frontend.
    Restart,
    /// A newer frontend instance replaced this one. Pending confirmations deny.
    Replaced,
}

/// User answer to a pending confirmation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmationDecision {
    /// Allow the pending action.
    Allow,
    /// Deny the pending action.
    Deny,
}

/// Reasoning effort as a frontend DTO. Mapped to Runtime on apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrontendReasoningEffort {
    /// Lowest effort tier.
    Minimal,
    /// Low effort.
    Low,
    /// Medium effort.
    Medium,
    /// High effort.
    High,
    /// Very high effort.
    VeryHigh,
    /// Maximum effort.
    Maximum,
}

/// One image (or other binary) attachment on submit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendAttachment {
    /// MIME type. Not interpreted by the service.
    pub media_type: String,
    /// Raw bytes. Mapped byte-for-byte onto `UserPart::Image`.
    pub bytes: Vec<u8>,
}

/// Commands the TUI (or oneshot frontend) may send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontendCommand {
    /// Admit a user turn on the service's current loaded session.
    Submit {
        /// Draft text. May be empty when attachments are present.
        draft: String,
        /// Optional image parts.
        attachments: Vec<FrontendAttachment>,
    },
    /// Cancel one operation. Control lane.
    CancelOperation {
        /// Runtime operation id.
        operation_id: String,
    },
    /// Start manual compaction.
    StartCompaction {
        /// Target session.
        session_id: String,
    },
    /// Cancel one maintenance task. Control lane.
    CancelMaintenance {
        /// Maintenance id.
        maintenance_id: String,
    },
    /// List durable sessions from the session store.
    ListSessions,
    /// Make `session_id` current and load its durable view.
    LoadSession {
        /// Session to load.
        session_id: String,
    },
    /// Read a session view without changing the current session.
    PreviewSession {
        /// Session to preview.
        session_id: String,
        /// Caller-owned generation used to discard stale previews.
        request_generation: u64,
    },
    /// Mint a new session id and make it current.
    CreateSession,
    /// Assemble and install a new model generation in the background.
    InstallModel {
        /// Requested model name.
        name: String,
    },
    /// Install a new generation that differs only by reasoning effort.
    SetReasoning {
        /// Requested effort.
        effort: FrontendReasoningEffort,
    },
    /// Read effective config entries (never secrets).
    ReadConfig,
    /// Read availability, generation display, and tool lineup.
    ReadStatus,
    /// Answer a pending confirmation. Control lane.
    RespondConfirmation {
        /// Confirmation id.
        confirmation_id: u64,
        /// User decision.
        decision: ConfirmationDecision,
    },
    /// Compose Session view + live snapshot. Reserved recovery lane.
    RequestSnapshot {
        /// Last revision the frontend applied.
        known_revision: FrontendRevision,
    },
    /// A frontend instance is attached.
    FrontendAttached {
        /// Instance identity.
        frontend_instance_id: FrontendInstanceId,
    },
    /// A frontend instance detached. Control lane. Does not cancel operations.
    FrontendDetached {
        /// Instance identity.
        frontend_instance_id: FrontendInstanceId,
        /// Why it detached.
        reason: DetachReason,
    },
    /// Request service/runtime shutdown. Control lane.
    ShutdownRequested,
}

impl FrontendCommand {
    /// Cancel, confirmation, shutdown, and detach use the control lane.
    pub fn is_control(&self) -> bool {
        matches!(
            self,
            Self::CancelOperation { .. }
                | Self::CancelMaintenance { .. }
                | Self::RespondConfirmation { .. }
                | Self::FrontendDetached { .. }
                | Self::ShutdownRequested
        )
    }

    /// Snapshot requests use the reserved recovery lane.
    pub fn is_snapshot(&self) -> bool {
        matches!(self, Self::RequestSnapshot { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_and_snapshot_identity() {
        assert!(FrontendCommand::ShutdownRequested.is_control());
        assert!(
            FrontendCommand::CancelOperation {
                operation_id: "op-1".into()
            }
            .is_control()
        );
        assert!(
            FrontendCommand::RequestSnapshot {
                known_revision: FrontendRevision::ZERO
            }
            .is_snapshot()
        );
        let submit = FrontendCommand::Submit {
            draft: "hi".into(),
            attachments: Vec::new(),
        };
        assert!(!submit.is_control());
        let FrontendCommand::Submit {
            draft: _,
            attachments: _,
        } = submit
        else {
            panic!("Submit must stay a two-field command without session_id");
        };
    }
}
