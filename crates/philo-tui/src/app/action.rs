//! Semantic input actions consumed by the pure app state machine.

use philo_agent_service::CommandReject;

use super::attachment::PendingAttachment;
use super::submit::{CancelDispatchResult, IntentId, SubmitDispatchResult};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    InsertChar(char),
    InsertNewline,
    Backspace,
    Delete,
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    Home,
    End,
    Submit,
    /// `Esc`: cancel while running, close overlays.
    Escape,
    /// `Ctrl+C`: clear input / cancel / two-step exit, by context.
    CtrlC,
    /// `Ctrl+D`: exit when the input is empty.
    CtrlD,
    /// `Ctrl+O`: toggle the information tier.
    ToggleLevel,
    /// `Ctrl+L`: force a full redraw.
    Redraw,
    /// `PageUp`: scroll the sealed transcript toward older rows.
    /// Kept as the browse-page engine's internal contract (P5 §6) — the
    /// keymap no longer binds PgUp/PgDn to it, and tests exercise the
    /// dispatch path directly.
    #[allow(dead_code)]
    PageTranscriptUp,
    /// `PageDown`: scroll the sealed transcript toward newer rows.
    /// Same P5 §6 rationale as `PageTranscriptUp`.
    #[allow(dead_code)]
    PageTranscriptDown,
    /// `PgUp` / `Ctrl+U` from the composer: leave the input (draft intact)
    /// and enter history browse mode.
    EnterBrowse,
    /// `k`/`↑` and `j`/`↓` in browse mode: step the logical cursor by one
    /// row toward older (`-1`) or newer (`+1`) history.
    BrowseStep(isize),
    /// `PgUp` / `PgDn` in browse mode: page the logical cursor by a whole
    /// viewport (`-1` toward older, `+1` toward newer).
    BrowsePage(isize),
    /// `Space` / `o` in browse mode: toggle the foldable element under the
    /// cursor — a tool-card body or a think header.
    BrowseToggleFold,
    /// `i` in browse mode (or `Esc` with no overlay open): return the focus
    /// to the composer without disturbing the scroll position.
    ExitBrowse,
    /// Wheel or equivalent: negative moves toward older rows.
    ScrollTranscript(isize),
    /// Left-button down inside the transcript/live band.
    SelectStart {
        x: u16,
        y: u16,
    },
    /// Pointer moved while a transcript selection is being dragged.
    SelectDrag {
        x: u16,
        y: u16,
    },
    /// Left-button up; a collapsed range is discarded.
    SelectEnd {
        x: u16,
        y: u16,
    },
    /// `Tab`: slash-command completion.
    Complete,
    /// `Ctrl+V` reached the app: the terminal did not turn it into a
    /// bracketed paste, so the clipboard has to be read directly.
    Paste,
    /// Media decode refused a pending submit intent.
    SubmitMediaRefused {
        intent_id: IntentId,
        kept: Vec<PendingAttachment>,
        errors: Vec<String>,
    },
    /// `try_command(Submit)` finished for a pending intent.
    SubmitDispatchFinished {
        intent_id: IntentId,
        result: SubmitDispatchResult,
    },
    /// Service refused a dequeued submit that still has a local pending intent.
    SubmitCommandRejected {
        intent_id: IntentId,
        reason: CommandReject,
    },
    /// Runtime admitted submit; commit local pending state.
    SubmitAccepted {
        intent_id: IntentId,
        operation_id: String,
    },
    /// `try_command(Cancel*)` finished.
    CancelDispatchFinished {
        request_id: u64,
        result: CancelDispatchResult,
    },
    /// `try_command(CancelMaintenance)` finished.
    CompactionCancelDispatchFinished {
        result: CancelDispatchResult,
    },
    None,
}
