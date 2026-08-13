//! Side-effect requests emitted by the pure app state machine.

use philo_agent_runtime::ReasoningEffort;
use philo_session::SessionId;

use crate::api::confirmation::{ConfirmationId, ConfirmationResponse};

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
    Submit(String),
    CancelActive,
    Quit,
    Redraw,
    Host(HostRequest),
}
