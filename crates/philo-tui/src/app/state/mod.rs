//! The interaction state machine: key actions and agent events go in,
//! append-only transcript lines and side-effect requests come out. Pure
//! state — the event loop owns the terminal and the host.

mod commands;
mod composer;
mod overlays;
mod runtime;
mod select;

#[cfg(test)]
mod tests;

use std::cell::Cell;
use std::collections::HashSet;

use super::action::Action;
use super::activity::{ActivityState, ActivityTone, ActivityView};
use super::attachment::Attachments;
use super::cells::{ScrollState, TranscriptStore, VisibleSlice};
use super::effect::Effect;
use super::input::{InputEditor, InputHistory};
use super::overlay::{ConfirmPrompt, OverlayFrame, SessionPicker};
use super::select::{BandLayout, Selection};
use super::status::StatusData;
use super::submit::SubmitState;
use super::transcript::{InfoLevel, LineKind, Transcript, TranscriptLine};

use commands::CommandMenu;
pub(crate) use commands::CommandMenuFrame;

pub(crate) use overlays::SessionLoadIntent;

/// Pure interaction state for one TUI session.
pub(crate) struct App {
    pub(crate) input: InputEditor,
    pub(crate) transcript: Transcript,
    pub(crate) status: StatusData,
    level: InfoLevel,
    history: InputHistory,
    /// One `Ctrl+C` on an idle, empty prompt arms the exit; the second
    /// quits. Any other action disarms.
    exit_armed: bool,
    /// `/quit` during a running turn asks once before leaving.
    quit_armed: bool,
    /// The session picker, while `/sessions` is open.
    picker: Option<SessionPicker>,
    /// The approval prompt, while a confirmation request is pending.
    confirm: Option<ConfirmPrompt>,
    /// The auto command menu, while the draft is a bare `/word`.
    completion: Option<CommandMenu>,
    /// Images waiting for the next message (`/image`, `Ctrl+V`).
    attachments: Attachments,
    /// Changes whenever draft contents are consumed or edited. Background
    /// media failures may restore only the exact draft generation they left.
    draft_generation: u64,
    /// Next submit intent id (monotonic).
    next_intent_id: crate::app::submit::IntentId,
    /// Local submit commit protocol (pending until `SubmitAccepted`).
    submit_state: SubmitState,
    /// `[ui].show_reasoning`, carried across session switches.
    show_reasoning: bool,
    /// How the next `SessionLoaded` should be presented.
    session_load_intent: Option<SessionLoadIntent>,
    /// `/config` is waiting for a listing rather than a hot-reload notice.
    expect_config_listing: bool,
    /// `/model` is waiting for install success or rejection.
    pending_model_switch: bool,
    /// Manual compaction has a standalone future owned by the driver.
    manual_compacting: bool,
    /// Automatic compaction belongs to the front operation handle.
    automatic_compacting: bool,
    /// Ephemeral operation projection; never enters transcript or Session.
    activity: ActivityState,
    /// Transcript cells for the TUI-owned viewport.
    pub(crate) cells: TranscriptStore,
    scroll: ScrollState,
    /// Think blocks the user manually expanded. Sealed blocks start folded.
    reasoning_manually_expanded: HashSet<usize>,
    /// Streaming think blocks the user manually folded before their seal.
    reasoning_manually_collapsed: HashSet<usize>,
    layout_width: Cell<usize>,
    layout_history_height: Cell<usize>,
    history_band: Cell<BandLayout>,
    selection: Option<Selection>,
}

impl App {
    pub fn new(status: StatusData, show_reasoning: bool) -> Self {
        let level = status.level;
        Self {
            input: InputEditor::new(),
            transcript: Transcript::new(show_reasoning),
            status,
            level,
            history: InputHistory::default(),
            exit_armed: false,
            quit_armed: false,
            picker: None,
            confirm: None,
            completion: None,
            attachments: Attachments::default(),
            draft_generation: 0,
            next_intent_id: 1,
            submit_state: SubmitState::Editing,
            show_reasoning,
            session_load_intent: None,
            expect_config_listing: false,
            pending_model_switch: false,
            manual_compacting: false,
            automatic_compacting: false,
            activity: ActivityState::default(),
            cells: TranscriptStore::new(),
            scroll: ScrollState::follow(),
            reasoning_manually_expanded: HashSet::new(),
            reasoning_manually_collapsed: HashSet::new(),
            layout_width: Cell::new(80),
            layout_history_height: Cell::new(0),
            history_band: Cell::new(BandLayout::default()),
            selection: None,
        }
    }

    pub(crate) fn history_slice(&self, width: usize, height: usize) -> VisibleSlice {
        self.cells
            .visible_slice(width, height, &self.scroll, &|index| {
                self.reasoning_collapsed(index)
            })
    }

    /// Fold state of the reasoning run starting at `head`: a streaming run
    /// stays open unless the user folded it; sealed runs start folded.
    fn reasoning_collapsed(&self, head: usize) -> bool {
        let open = self.cells.open_cell();
        let streaming_run = open.is_some_and(|open| {
            open >= head
                && self.cells.display_kind(head) == LineKind::Reasoning
                && (head..=open).all(|index| self.cells.display_kind(index) == LineKind::Reasoning)
        });
        if streaming_run {
            self.reasoning_manually_collapsed.contains(&head)
        } else {
            !self.reasoning_manually_expanded.contains(&head)
        }
    }

    /// Click on a think header row: fold/unfold that block. Returns whether
    /// the position actually was a reasoning-run head.
    pub(crate) fn toggle_reasoning_block(&mut self, cell: usize, row: usize) -> bool {
        if row != 0 || self.cells.display_kind(cell) != LineKind::Reasoning {
            return false;
        }
        let is_head = cell == 0 || self.cells.display_kind(cell - 1) != LineKind::Reasoning;
        if !is_head {
            return false;
        }
        let collapsed_now = self.reasoning_collapsed(cell);
        self.reasoning_manually_expanded.remove(&cell);
        self.reasoning_manually_collapsed.remove(&cell);
        if collapsed_now {
            self.reasoning_manually_expanded.insert(cell);
        } else {
            self.reasoning_manually_collapsed.insert(cell);
        }
        self.cells.bump_wrap_revision();
        true
    }

    /// Copies every `Effect::Append` into the store as closed cells.
    /// Callers still return the original effects so existing collectors keep
    /// working. Agent events write the store through [`Transcript::apply`]
    /// and must not be ingested again.
    pub(crate) fn ingest_appends(&mut self, effects: Vec<Effect>) -> Vec<Effect> {
        for effect in &effects {
            if let Effect::Append(lines) = effect {
                self.cells.push_closed(lines.clone());
            }
        }
        effects
    }

    pub(crate) fn page_transcript_up(&mut self, width: usize, height: usize) {
        self.scroll_transcript(width, height, -(height as isize));
    }

    pub(crate) fn page_transcript_down(&mut self, width: usize, height: usize) {
        self.scroll_transcript(width, height, height as isize);
    }

    pub(crate) fn scroll_transcript(&mut self, width: usize, height: usize, delta: isize) {
        if height == 0 || delta == 0 {
            return;
        }
        self.cells
            .refresh_wraps(width, &|index| self.reasoning_collapsed(index));
        self.scroll
            .scroll_wrapped(&self.cells.wrap_rows(), height, delta);
    }

    pub(crate) fn jump_transcript_top(&mut self) {
        let width = self.layout_width.get();
        let height = self.layout_history_height.get();
        self.cells
            .refresh_wraps(width, &|index| self.reasoning_collapsed(index));
        self.scroll.jump_top(&self.cells.wrap_rows(), height);
    }

    pub(crate) fn jump_transcript_bottom(&mut self) {
        self.scroll.jump_bottom();
    }

    #[cfg(test)]
    pub(crate) fn level(&self) -> InfoLevel {
        self.level
    }

    #[cfg(test)]
    pub(crate) fn shows_reasoning(&self) -> bool {
        self.show_reasoning
    }

    #[cfg(test)]
    pub(crate) fn follow_bottom(&self) -> bool {
        self.scroll.follow_bottom()
    }

    #[cfg(test)]
    pub(crate) fn has_selection(&self) -> bool {
        self.clamped_selection().is_some()
    }

    /// Images waiting for the next message (`/image`, `Ctrl+V`).
    pub fn attachments(&self) -> &Attachments {
        &self.attachments
    }

    /// The overlay content to paint, if any. The approval prompt wins over
    /// the session picker: an answer is what unblocks the running turn.
    #[cfg(test)]
    pub fn overlay_frame(&self, height: usize) -> Option<OverlayFrame> {
        if let Some(confirm) = &self.confirm {
            return Some(confirm.frame(height));
        }
        self.picker.as_ref().map(|picker| picker.frame(height))
    }

    pub(crate) fn overlay_frame_for(&self, height: usize, width: usize) -> Option<OverlayFrame> {
        if let Some(confirm) = &self.confirm {
            return Some(confirm.frame_for(height, width));
        }
        self.picker
            .as_ref()
            .map(|picker| picker.frame_for(height, width))
    }

    pub(crate) fn activity_view(&self, width: usize) -> Option<ActivityView> {
        if self.confirm.is_some() {
            return Some(ActivityView {
                text: super::text::truncate("! Approval required", width),
                tone: ActivityTone::Warning,
            });
        }
        self.activity.view(width)
    }

    pub(crate) fn activity_detail_rows(&self, width: usize, height: usize) -> Vec<String> {
        if self.confirm.is_some() || self.picker.is_some() {
            return Vec::new();
        }
        let tail_lines = match self.level {
            InfoLevel::Verbose => 20,
            InfoLevel::Default => 5,
        };
        self.activity.detail_rows(width, height, tail_lines)
    }

    pub(crate) fn activity_timeline_row(&self, width: usize) -> Option<String> {
        if self.confirm.is_some() || self.picker.is_some() {
            return None;
        }
        self.activity.timeline_row(width)
    }

    pub(crate) fn has_overlay(&self) -> bool {
        self.confirm.is_some() || self.picker.is_some()
    }

    pub(crate) fn input_focused(&self) -> bool {
        self.confirm.is_none() && self.picker.is_none()
    }

    /// The menu frame to paint above the composer, while it is open.
    pub(crate) fn command_menu_frame(
        &self,
        width: usize,
        max_rows: usize,
    ) -> Option<CommandMenuFrame> {
        self.completion
            .as_ref()
            .map(|menu| menu.frame(width, max_rows))
    }

    /// Handles one interpreted key action.
    pub fn on_action(&mut self, action: Action) -> Vec<Effect> {
        let effects = self.dispatch_action(action);
        self.ingest_appends(effects)
    }

    fn dispatch_action(&mut self, action: Action) -> Vec<Effect> {
        // Key-release events under the kitty protocol surface as `None`;
        // they are fully inert: no exit disarm, no menu churn.
        if matches!(action, Action::None) {
            return vec![];
        }
        if self.confirm.is_some() {
            return self.on_confirm_action(action);
        }
        if self.picker.is_some() {
            return self.on_picker_action(action);
        }
        if self.completion.is_some() {
            match action {
                Action::Escape => {
                    self.completion = None;
                    return vec![];
                }
                Action::MoveUp => return self.move_completion(true),
                Action::MoveDown => return self.move_completion(false),
                Action::Submit => return self.execute_completion(),
                Action::Complete => return self.accept_completion(),
                _ => {}
            }
        }
        // Any interaction other than the quit chord disarms the two-step
        // exit; anything but another `/quit` disarms the running-turn exit.
        if !matches!(action, Action::CtrlC) {
            self.exit_armed = false;
        }
        match action {
            Action::InsertChar(ch) => self.insert_char(ch),
            Action::InsertNewline => self.insert_newline(),
            Action::Backspace => self.backspace(),
            Action::Delete => self.delete(),
            Action::MoveLeft => self.move_left(),
            Action::MoveRight => self.move_right(),
            Action::Home => self.home(),
            Action::End => self.end(),
            Action::MoveUp => self.move_up(),
            Action::MoveDown => self.move_down(),
            Action::Submit => self.submit(),
            Action::Escape => self.escape(),
            Action::CtrlC => self.ctrl_c(),
            Action::CtrlD => {
                if self.input.is_empty() {
                    vec![Effect::Quit]
                } else {
                    vec![]
                }
            }
            Action::ToggleLevel => vec![Effect::Append(vec![self.toggle_level()])],
            Action::Redraw => vec![Effect::HardRedraw],
            Action::PageTranscriptUp => {
                self.page_transcript_up(self.layout_width.get(), self.layout_history_height.get());
                vec![]
            }
            Action::PageTranscriptDown => {
                self.page_transcript_down(
                    self.layout_width.get(),
                    self.layout_history_height.get(),
                );
                vec![]
            }
            Action::ScrollTranscript(delta) => {
                self.scroll_transcript(
                    self.layout_width.get(),
                    self.layout_history_height.get(),
                    delta,
                );
                vec![]
            }
            Action::SelectStart { x, y } => self.select_start(x, y),
            Action::SelectDrag { x, y } => self.select_drag(x, y),
            Action::SelectEnd { x, y } => self.select_end(x, y),
            Action::Complete => self.complete(),
            Action::Paste => vec![Effect::ReadClipboard],
            Action::SubmitMediaRefused {
                intent_id,
                kept,
                errors,
            } => self.on_submit_media_refused(intent_id, kept, errors),
            Action::SubmitDispatchFinished { intent_id, result } => {
                self.on_submit_dispatch_finished(intent_id, result)
            }
            Action::SubmitCommandRejected { intent_id, reason } => {
                self.on_submit_command_rejected(intent_id, reason)
            }
            Action::SubmitAccepted {
                intent_id,
                operation_id,
            } => self.on_submit_accepted(intent_id, operation_id),
            Action::CancelDispatchFinished { .. } => {
                // Interrupt FSM lives in the driver; reducer only shows copy
                // when the driver feeds Append effects alongside this action.
                vec![]
            }
            Action::CompactionCancelDispatchFinished { result } => {
                self.on_compaction_cancel_dispatch_finished(result)
            }
            Action::None => vec![],
        }
    }

    pub(crate) fn submit_state(&self) -> &SubmitState {
        &self.submit_state
    }

    fn has_activity(&self) -> bool {
        self.status.busy || self.status.compacting
    }

    fn bump_draft_generation(&mut self) {
        self.draft_generation = self.draft_generation.wrapping_add(1);
    }
}

fn line(kind: LineKind, text: impl Into<String>) -> TranscriptLine {
    TranscriptLine {
        kind,
        text: text.into(),
    }
}
