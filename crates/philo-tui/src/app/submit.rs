//! Submit intent: draft is held until Service admits the command.

use philo_agent_service::FrontendRequestId;

use super::attachment::PendingAttachment;

/// Monotonic id for one user submit intent.
pub(crate) type IntentId = u64;

/// Draft held while media decode / command dispatch is in flight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingSubmission {
    pub(crate) intent_id: IntentId,
    pub(crate) draft: String,
    pub(crate) attachments: Vec<PendingAttachment>,
    /// Set when `try_command(Submit)` returns `Enqueued`.
    pub(crate) request_id: Option<FrontendRequestId>,
    /// `draft_generation` at the moment the intent entered `Dispatching`.
    pub(crate) held_generation: u64,
}

/// Local submit commit state. At most one pending intent per TUI instance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum SubmitState {
    #[default]
    Editing,
    Dispatching(PendingSubmission),
    Accepted {
        intent_id: IntentId,
        operation_id: String,
    },
}

impl SubmitState {
    pub(crate) fn intent_id(&self) -> Option<IntentId> {
        match self {
            Self::Editing => None,
            Self::Dispatching(pending) => Some(pending.intent_id),
            Self::Accepted { intent_id, .. } => Some(*intent_id),
        }
    }

    pub(crate) fn pending(&self) -> Option<&PendingSubmission> {
        match self {
            Self::Dispatching(pending) => Some(pending),
            _ => None,
        }
    }

    pub(crate) fn pending_mut(&mut self) -> Option<&mut PendingSubmission> {
        match self {
            Self::Dispatching(pending) => Some(pending),
            _ => None,
        }
    }
}

/// Structured result of `try_command(Submit)` for the reducer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SubmitDispatchResult {
    Enqueued(FrontendRequestId),
    Backpressured,
    Disconnected { lane: &'static str },
}

/// Structured result of `try_command(Cancel*)` for the reducer / interrupt FSM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CancelDispatchResult {
    Enqueued(FrontendRequestId),
    Backpressured,
    Disconnected { lane: &'static str },
}
