//! Side-effect requests emitted by the pure app state machine.

use philo_agent_service::{ConfirmationDecision, FrontendReasoningEffort};

use super::attachment::PendingAttachment;
use super::submit::IntentId;
use super::transcript::TranscriptLine;

/// One service-backed operation requested by a command or overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HostRequest {
    NewSession,
    OpenSessions,
    OpenModels,
    LoadPreview(String),
    SwitchSession(String),
    RenameSession { title: String },
    RebuildModel(String),
    SetReasoning(FrontendReasoningEffort),
    ShowConfig,
    ShowStatus,
    Respond(u64, ConfirmationDecision),
}

/// A side effect the driver must perform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    Append(Vec<TranscriptLine>),
    /// Prepare and dispatch a pending submit intent (media decode then
    /// `try_command`). Local draft stays in AppState until admitted.
    PrepareSubmit {
        intent_id: IntentId,
        text: String,
        attachments: Vec<PendingAttachment>,
    },
    /// `Ctrl+V` outside bracketed paste: read the system clipboard.
    ReadClipboard,
    /// Copy the TUI selection to the system clipboard.
    WriteClipboard(String),
    CancelActive,
    /// Busy `Ctrl+C`: counts toward the supervisor two-strike force-exit.
    /// Escape uses [`Effect::CancelActive`] and does not count.
    InterruptCancel,
    /// Ask the service to start manual compaction.
    StartCompaction,
    /// Ask the service to cancel the active maintenance task.
    CancelCompaction,
    /// Idle user exit. Supervisor detaches after `TuiOutcome::UserExit`.
    Quit,
    /// Busy `/quit` confirmation: `ShutdownRequested`.
    RequestShutdown,
    /// Explicit terminal recovery (`Ctrl+L`), not ordinary invalidation.
    HardRedraw,
    Host(HostRequest),
}
