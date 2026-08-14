//! Side-effect requests emitted by the pure app state machine.

use philo_agent_runtime::ReasoningEffort;
use philo_session::SessionId;

use crate::api::confirmation::{ConfirmationId, ConfirmationResponse};

use super::attachment::PendingAttachment;
use super::transcript::TranscriptLine;

/// One host-backed operation requested by a command or overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HostRequest {
    NewSession,
    OpenSessions,
    LoadPreview(SessionId),
    SwitchSession(SessionId),
    RebuildModel(String),
    SetReasoning(ReasoningEffort),
    ShowConfig,
    ShowStatus,
    Respond(ConfirmationId, ConfirmationResponse),
}

/// A side effect the driver must perform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    Append(Vec<TranscriptLine>),
    /// Send this message; the driver resolves the attachments into image
    /// parts first and refuses the send when one cannot be read.
    Submit {
        text: String,
        attachments: Vec<PendingAttachment>,
    },
    /// `Ctrl+V` outside bracketed paste: read the system clipboard.
    ReadClipboard,
    CancelActive,
    /// Start a cancellable manual compaction in the driver's select loop.
    StartCompaction,
    /// Drop the active manual compaction future.
    CancelCompaction,
    Quit,
    Redraw,
    Host(HostRequest),
}
