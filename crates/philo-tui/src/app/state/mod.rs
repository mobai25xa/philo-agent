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
use super::attachment::Attachments;
use super::cells::{ScrollState, TranscriptStore, VisibleSlice};
use super::effect::Effect;
use super::input::{InputEditor, InputHistory};
use super::overlay::{ConfirmPrompt, OverlayFrame, Picker};
use super::pacer::{PacedPiece, StreamPacer};
use super::run_state::{CornerWord, RunState};
use super::select::{BandLayout, Selection};
use super::status::StatusData;
use super::submit::SubmitState;
use super::transcript::{InfoLevel, LineKind, Transcript, TranscriptLine};

use commands::CommandMenu;
pub(crate) use commands::CommandMenuFrame;

pub(crate) use overlays::SessionLoadIntent;

/// Streaming viewport anchor (v2.2, plan T4.7–T4.9): lifted output grows
/// from the 40% line and pins at the 80% line while busy; after settlement
/// the viewport animates back down to the full band.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamAnchor {
    /// Captured the wrapped-row total at lift time; appended rows grow the
    /// visible window from the 40% base toward the 80% cap.
    Lift { base_total: usize },
    /// Post-settlement drop animation: ticks remaining until the viewport
    /// reaches the full band again.
    Settle { left: u16 },
}

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
    /// The session/model picker, while `/sessions` or `/models` is open.
    picker: Option<Picker>,
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
    /// `/models` is waiting for a catalog listing that should open the
    /// picker; silent refreshes (startup, post-install) leave it unset.
    expect_models_picker: bool,
    /// `/model` is waiting for install success or rejection.
    pending_model_switch: bool,
    /// Manual compaction has a standalone future owned by the driver.
    manual_compacting: bool,
    /// Automatic compaction belongs to the front operation handle.
    automatic_compacting: bool,
    /// Ephemeral run-state word; never enters transcript or Session.
    run_state: RunState,
    /// Stream smoothing valve (v2.2): live deltas queue here and animation
    /// ticks release them into the cells at an even cadence.
    pacer: StreamPacer,
    /// Streaming viewport anchor state (v2.2); `None` means the plain
    /// full-band, bottom-follow layout.
    stream_anchor: Option<StreamAnchor>,
    /// Last rendered full-frame height — the 40%/80% anchors are shares of
    /// it. Written by the render pass through a cell, like the band layout.
    pub(crate) frame_height: Cell<u16>,
    /// Transcript cells for the TUI-owned viewport.
    pub(crate) cells: TranscriptStore,
    scroll: ScrollState,
    /// Think blocks the user manually expanded. Every reasoning run —
    /// streaming or sealed (v2.2) — starts folded; only this set opens one.
    reasoning_manually_expanded: HashSet<usize>,
    layout_width: Cell<usize>,
    layout_history_height: Cell<usize>,
    history_band: Cell<BandLayout>,
    /// Full transcript-band height (independent of the painted sub-area,
    /// which shrinks when sparse content hangs from its tail).
    band_height: Cell<u16>,
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
            expect_models_picker: false,
            pending_model_switch: false,
            manual_compacting: false,
            automatic_compacting: false,
            run_state: RunState::default(),
            pacer: StreamPacer::default(),
            stream_anchor: None,
            frame_height: Cell::new(0),
            cells: TranscriptStore::new(),
            scroll: ScrollState::follow(),
            reasoning_manually_expanded: HashSet::new(),
            layout_width: Cell::new(80),
            layout_history_height: Cell::new(0),
            history_band: Cell::new(BandLayout::default()),
            band_height: Cell::new(0),
            selection: None,
        }
    }

    pub(crate) fn history_slice(&self, width: usize, height: usize) -> VisibleSlice {
        self.cells
            .visible_slice(width, height, &self.scroll, &|index| {
                self.reasoning_collapsed(index)
            })
    }

    /// Fold state of the reasoning run starting at `head` (v2.2): every
    /// run — streaming or sealed — starts folded; the header (with its
    /// live or frozen duration) is all that renders until the user
    /// expands it.
    fn reasoning_collapsed(&self, head: usize) -> bool {
        !self.reasoning_manually_expanded.contains(&head)
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
        // Default is folded, so an expanded run can only be one this set
        // holds: toggle by inserting/removing.
        if !self.reasoning_manually_expanded.remove(&cell) {
            self.reasoning_manually_expanded.insert(cell);
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

    // -- Stream pacing (v2.2, plan T4.11) ---------------------------------

    /// Queues one live streaming delta for even-cadence display.
    pub(crate) fn pace_delta(&mut self, kind: LineKind, text: &str) {
        self.pacer.push(kind, text);
    }

    /// Emits any buffered stream text into the transcript immediately, in
    /// order. Every structural boundary calls this before applying its own
    /// event — cancellation reveals everything at once and settlement sees
    /// exactly what an unpaced run would have seen.
    pub(crate) fn flush_stream(&mut self) -> bool {
        let pieces = self.pacer.flush();
        self.write_paced(pieces)
    }

    /// Drops buffered text without displaying (session switches).
    pub(crate) fn clear_stream(&mut self) {
        self.pacer.clear();
    }

    fn write_paced(&mut self, pieces: Vec<PacedPiece>) -> bool {
        if pieces.is_empty() {
            return false;
        }
        for piece in pieces {
            self.transcript
                .write_stream_piece(&mut self.cells, piece.kind, &piece.text);
        }
        true
    }

    // -- Streaming viewport anchors (v2.2, plan T4.7–T4.10) ---------------

    /// Ticks the post-settlement drop animation spreads over (~500ms at the
    /// 100ms animation cadence).
    pub(crate) const SETTLE_ANIM_TICKS: u16 = 5;

    /// Render pass records the full-frame height so event-side lift logic
    /// can evaluate the 40%/80% shares without owning geometry.
    pub(crate) fn note_frame_height(&self, height: u16) {
        self.frame_height.set(height);
    }

    /// Whether the streaming viewport (lift/pin/settle) currently owns the
    /// transcript window.
    pub(crate) fn stream_anchor_active(&self) -> bool {
        self.stream_anchor.is_some()
    }

    /// Visible transcript height for this frame: the full column except
    /// while streaming is lifted (`base + grown`, capped at the 80% line),
    /// busy-follow pins at the cap, and the settle animation walks back to
    /// full. Manual scroll cancels the anchor entirely.
    pub(crate) fn transcript_viewport_height(&self, full: u16, anchors: Option<(u16, u16)>) -> u16 {
        let Some((base, cap)) = anchors else {
            return full;
        };
        let cap = cap.min(full);
        let base = base.min(cap.saturating_sub(1)).max(1);
        let height = match self.stream_anchor {
            Some(StreamAnchor::Lift { base_total }) => {
                let grown = self.wrapped_row_total().saturating_sub(base_total);
                u16::try_from(usize::from(base) + grown)
                    .unwrap_or(u16::MAX)
                    .min(cap)
            }
            Some(StreamAnchor::Settle { left }) => {
                let span = full - cap;
                let done = Self::SETTLE_ANIM_TICKS.saturating_sub(left);
                cap + span * done / Self::SETTLE_ANIM_TICKS
            }
            None if self.status.busy => cap,
            None => full,
        };
        height.clamp(1, full)
    }

    /// Total wrapped rows at the last-known width (the lift's growth ruler).
    fn wrapped_row_total(&self) -> usize {
        let width = self.layout_width.get();
        self.cells
            .refresh_wraps(width, &|index| self.reasoning_collapsed(index));
        self.cells.wrap_rows().iter().map(Vec::len).sum()
    }

    /// Lifts the viewport to the 40% line for a new turn: only when the
    /// screen shows content (blank screens start at the top), the user is
    /// following, the layout is known, and the anchors fit.
    fn try_begin_stream_lift(&mut self) {
        if self.stream_anchor.is_some() || !self.scroll.follow_bottom() {
            return;
        }
        if self.cells.display_len() == 0 {
            return;
        }
        if crate::render::stream_anchor_rows(self.frame_height.get(), self.band_height.get())
            .is_none()
        {
            return;
        }
        let base_total = self.wrapped_row_total();
        self.stream_anchor = Some(StreamAnchor::Lift { base_total });
    }

    /// Settlement ends the lift: animate the tail down to the band bottom.
    /// Turns that never lifted simply stay where they are.
    pub(crate) fn begin_settle_drop(&mut self) {
        if matches!(self.stream_anchor, Some(StreamAnchor::Lift { .. })) {
            self.stream_anchor = Some(StreamAnchor::Settle {
                left: Self::SETTLE_ANIM_TICKS,
            });
        }
    }

    /// User scrolling takes over: drop any lift/settle animation so the
    /// automatic anchoring never fights the operator.
    fn cancel_stream_anchor(&mut self) {
        self.stream_anchor = None;
    }

    pub(crate) fn page_transcript_up(&mut self, width: usize, height: usize) {
        self.cancel_stream_anchor();
        self.scroll_transcript(width, height, -(height as isize));
    }

    pub(crate) fn page_transcript_down(&mut self, width: usize, height: usize) {
        self.cancel_stream_anchor();
        self.scroll_transcript(width, height, height as isize);
    }

    pub(crate) fn scroll_transcript(&mut self, width: usize, height: usize, delta: isize) {
        if delta == 0 {
            return;
        }
        self.cancel_stream_anchor();
        self.cells
            .refresh_wraps(width, &|index| self.reasoning_collapsed(index));
        self.scroll
            .scroll_wrapped(&self.cells.wrap_rows(), height, delta);
    }

    pub(crate) fn jump_transcript_top(&mut self) {
        self.cancel_stream_anchor();
        let width = self.layout_width.get();
        let height = self.layout_history_height.get();
        self.cells
            .refresh_wraps(width, &|index| self.reasoning_collapsed(index));
        self.scroll.jump_top(&self.cells.wrap_rows(), height);
    }

    pub(crate) fn jump_transcript_bottom(&mut self) {
        self.cancel_stream_anchor();
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
    ///
    /// The parameters' meaning depends on which overlay is live: the
    /// approval keeps them as caps around its content-sized panel, while a
    /// picker treats them as the exact fixed dialog targets (v0.44 §4.2).
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

    /// Whether an approval prompt currently owns the overlay slot. Pickers
    /// float at fixed size instead; the renderer picks the matching budget.
    pub(crate) fn has_confirmation(&self) -> bool {
        self.confirm.is_some()
    }

    /// Composer top-left corner (§2.4): the run-state word with the
    /// `Approval…` overlay flag applied while a confirmation is pending.
    /// The flag hides the underlying word without replacing it.
    pub(crate) fn run_state_corner(&self, max_width: usize) -> Option<CornerWord> {
        self.run_state.corner(max_width, self.confirm.is_some())
    }

    #[cfg(test)]
    pub(crate) fn run_state_mut(&mut self) -> &mut RunState {
        &mut self.run_state
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
        tone: crate::app::transcript::Tone::Plain,
    }
}
