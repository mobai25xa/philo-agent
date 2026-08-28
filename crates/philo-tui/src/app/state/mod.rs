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
use std::collections::{HashMap, HashSet};

use super::action::Action;
use super::attachment::Attachments;
use super::cells::{
    ScrollState, TranscriptStore, VisibleSlice, clamp_cursor, logical_index, move_cursor,
    position_at_index, start_from_tail,
};
use super::effect::Effect;
use super::input::{InputEditor, InputHistory};
use super::overlay::{ConfirmPrompt, OverlayFrame, Picker};
use super::pacer::{PacedPiece, StreamPacer};
use super::run_state::{CornerWord, RunState};
use super::select::{BandLayout, Selection};
use philo_agent_service::{FrontendGenerationChoice, FrontendTokenUsage};
use super::status::StatusData;
use super::submit::SubmitState;
use super::transcript::{InfoLevel, LineKind, Transcript, TranscriptLine};

use commands::CommandMenu;
pub(crate) use commands::CommandMenuFrame;

pub(crate) use overlays::SessionLoadIntent;

/// Keyboard focus owner (P5 §1, new-tui.md §3): the composer by default,
/// the history browse mode while the user inspects old rows. The two
/// transient overlays (confirmation, pickers) have their own routing
/// priority above both and never set this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FocusMode {
    Input,
    Browse,
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
    /// Per-session token-usage cache so switching back to a history session
    /// restores the right-bottom telemetry to its last value. In-process
    /// only; cleared on compaction; survives a session round-trip but not a
    /// process restart.
    usage_cache: HashMap<String, FrontendTokenUsage>,
    /// Per-session generation choice cache so switching back to a history
    /// session restores the top-right model/effort corner to its last value.
    /// In-process only; survives a session round-trip but not a process
    /// restart (cross-process restore reads `DurableSessionView.generation`).
    model_cache: HashMap<String, FrontendGenerationChoice>,
    /// Transcript cells for the TUI-owned viewport.
    pub(crate) cells: TranscriptStore,
    scroll: ScrollState,
    /// Think blocks the user manually expanded. Every reasoning run —
    /// streaming or sealed (v2.2) — starts folded; only this set opens one.
    reasoning_manually_expanded: HashSet<usize>,
    /// The in-flight default-mode tool batch (v4.0 P3): one live card cell
    /// or a tree cell, rewritten in place as its events land. `None` while
    /// idle, verbose, or settled.
    tool_batch: Option<super::live_tool::LiveBatch>,
    /// Tool-card bodies the user manually expanded (v4.0 P3 §6 state API).
    tool_cards_expanded: HashSet<usize>,
    /// Tool-card bodies the user manually folded.
    tool_cards_folded: HashSet<usize>,
    layout_width: Cell<usize>,
    layout_history_height: Cell<usize>,
    history_band: Cell<BandLayout>,
    /// Full transcript-band height (independent of the painted sub-area,
    /// which shrinks when sparse content lays out from the band top).
    band_height: Cell<u16>,
    /// Wall clock of the most recent transcript scroll (P2 scrollbar heat).
    /// `None` once the 800 ms highlight window elapsed.
    scroll_hot_until: Cell<Option<std::time::Instant>>,
    selection: Option<Selection>,
    /// Keyboard focus owner (P5 §1): the composer, or history browse mode.
    focus_mode: FocusMode,
    /// Logical cursor `(cell, row_in_cell)` while browse mode owns the
    /// focus — the row Space/o hit-tests against foldable elements.
    browse_cursor: (usize, usize),
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
            usage_cache: HashMap::new(),
            model_cache: HashMap::new(),
            cells: TranscriptStore::new(),
            scroll: ScrollState::follow(),
            reasoning_manually_expanded: HashSet::new(),
            tool_batch: None,
            tool_cards_expanded: HashSet::new(),
            tool_cards_folded: HashSet::new(),
            layout_width: Cell::new(80),
            layout_history_height: Cell::new(0),
            history_band: Cell::new(BandLayout::default()),
            band_height: Cell::new(0),
            scroll_hot_until: Cell::new(None),
            selection: None,
            focus_mode: FocusMode::Input,
            browse_cursor: (0, 0),
        }
    }

    pub(crate) fn history_slice(&self, width: usize, height: usize) -> VisibleSlice {
        self.cells
            .visible_slice(width, height, &self.scroll, &|index| {
                self.collapser(index)
            })
    }

    /// Scrollbar inputs (P2 §1.3): total wrapped rows, top offset, and
    /// whether a scroll landed recently enough to light the thumb. Computed
    /// against the live wrap cache; the render pass calls this right after
    /// `history_slice` so the cache is already fresh.
    pub(crate) fn scrollbar_metrics(&self, width: usize, height: usize) -> (usize, usize) {
        self.cells.refresh_wraps(width, &|index| {
            self.collapser(index)
        });
        let wrapped = self.cells.wrap_rows();
        let total = wrapped.iter().map(Vec::len).sum();
        (total, self.scroll.scroll_offset(&wrapped, height))
    }

    /// Fold state of the reasoning run starting at `head` (v2.2): every
    /// run — streaming or sealed — starts folded; the header (with its
    /// live or frozen duration) is all that renders until the user
    /// expands it.
    fn reasoning_collapsed(&self, head: usize) -> bool {
        !self.reasoning_manually_expanded.contains(&head)
    }

    /// Combined fold state handed to the wrap cache: reasoning runs fold by
    /// their run head, tool-card bodies by their own cell (v4.0 P3 §6).
    fn collapser(&self, index: usize) -> bool {
        match self.cells.display_kind(index) {
            LineKind::Reasoning => self.reasoning_collapsed(index),
            LineKind::Tool => self.tool_card_collapsed(index),
            _ => false,
        }
    }

    /// Fold state of a tool-card body cell: cards settle folded past their
    /// threshold by default; the manual sets override that default.
    fn tool_card_collapsed(&self, index: usize) -> bool {
        let Some(line) = self.cells.cell_at(index) else {
            return false;
        };
        let Some(body) = &line.body else {
            return false;
        };
        if body.lines.len() <= body.threshold {
            return false;
        }
        if self.tool_cards_folded.contains(&index) {
            return true;
        }
        if self.tool_cards_expanded.contains(&index) {
            return false;
        }
        body.fold_default_collapsed
    }

    /// Fold/unfold a tool-card body by its cell index. Returns whether the
    /// cell actually was a foldable card (P3 §6 state API; Space/o wiring
    /// belongs to P5 — exercised by tests until then).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn toggle_tool_card_fold(&mut self, index: usize) -> bool {
        let Some(line) = self.cells.cell_at(index) else {
            return false;
        };
        let Some(body) = &line.body else {
            return false;
        };
        if body.lines.len() <= body.threshold {
            return false;
        }
        if body.fold_default_collapsed {
            if !self.tool_cards_expanded.remove(&index) {
                self.tool_cards_expanded.insert(index);
            }
        } else if !self.tool_cards_folded.remove(&index) {
            self.tool_cards_folded.insert(index);
        }
        self.cells.bump_wrap_revision();
        true
    }

    /// Whether the tool-card body at `index` is currently folded (the
    /// default applies until the user flips the manual sets).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn tool_card_collapsed_at(&self, index: usize) -> bool {
        self.tool_card_collapsed(index)
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

    // -- Streaming viewport policy (v4.1) ---------------------------------

    // The v2.2 40%/80% lift/pin/settle anchors are retired: the transcript
    // viewport is now the whole band. Sparse content lays out from the band
    // top; once it overflows, the follow-bottom slice naturally pushes new
    // rows in from the bottom edge. No state machine is needed.

    pub(crate) fn page_transcript_up(&mut self, width: usize, height: usize) {
        self.scroll_transcript(width, height, -(height as isize));
    }

    pub(crate) fn page_transcript_down(&mut self, width: usize, height: usize) {
        self.scroll_transcript(width, height, height as isize);
    }

    pub(crate) fn scroll_transcript(&mut self, width: usize, height: usize, delta: isize) {
        if delta == 0 {
            return;
        }
        self.note_scroll_activity();
        self.cells
            .refresh_wraps(width, &|index| self.collapser(index));
        self.scroll
            .scroll_wrapped(&self.cells.wrap_rows(), height, delta);
    }

    pub(crate) fn jump_transcript_top(&mut self) {
        self.note_scroll_activity();
        let width = self.layout_width.get();
        let height = self.layout_history_height.get();
        self.cells
            .refresh_wraps(width, &|index| self.collapser(index));
        self.scroll.jump_top(&self.cells.wrap_rows(), height);
    }

    pub(crate) fn jump_transcript_bottom(&mut self) {
        self.note_scroll_activity();
        self.scroll.jump_bottom();
    }

    /// Marks the scrollbar thumb "recently scrolled" (P2 §1.2): the 800 ms
    /// highlight window starts now. A driving animation deadline exists
    /// whenever the window is open.
    fn note_scroll_activity(&self) {
        self.scroll_hot_until.set(Some(
            std::time::Instant::now()
                + std::time::Duration::from_millis(crate::render::theme::SCROLL_ACTIVE_MS),
        ));
    }

    /// Whether the thumb highlight is currently lit; also clears the
    /// stale timestamp so idle rails stop asking for frames.
    pub(crate) fn scrollbar_active(&self) -> bool {
        match self.scroll_hot_until.get() {
            Some(until) if std::time::Instant::now() < until => true,
            Some(_) => {
                self.scroll_hot_until.set(None);
                false
            }
            None => false,
        }
    }

    // -- History browse mode (P5 §2) --------------------------------------

    pub(crate) fn focus_mode(&self) -> FocusMode {
        self.focus_mode
    }

    /// Whether history browse mode currently owns the keyboard focus.
    pub(crate) fn in_browse_mode(&self) -> bool {
        self.focus_mode == FocusMode::Browse
    }

    /// The logical cursor position while browse mode is active — the row
    /// the renderer lifts with the MENU_ACTIVE_BG tint. `None` outside
    /// browse mode.
    pub(crate) fn browse_cursor(&self) -> Option<(usize, usize)> {
        self.in_browse_mode().then_some(self.browse_cursor)
    }

    /// `PgUp` / `Ctrl+U`: leave the composer and take the current scroll
    /// position over as the browse origin. The draft, attachments and input
    /// cursor survive untouched; a pending command menu closes. The view
    /// pins here so live output never yanks the reading place.
    fn enter_browse(&mut self) {
        if self.focus_mode == FocusMode::Browse {
            return;
        }
        self.clear_selection();
        self.completion = None;
        let width = self.layout_width.get();
        let height = self.layout_history_height.get();
        self.cells
            .refresh_wraps(width, &|index| self.collapser(index));
        if !self.cells.wrap_rows().is_empty() && height > 0 {
            let wrapped = self.cells.wrap_rows();
            self.scroll.unfollow_keep_wrapped(&wrapped, height);
        }
        self.focus_mode = FocusMode::Browse;
        let wrapped = self.cells.wrap_rows();
        self.browse_cursor = if self.scroll.follow_bottom() && !wrapped.is_empty() {
            start_from_tail(&wrapped, height)
        } else {
            self.scroll.pin().unwrap_or((0, 0))
        };
    }

    /// `i` / `Esc`: return the focus to the composer. The scroll position
    /// is preserved — the reading place survives the round-trip.
    fn exit_browse(&mut self) {
        if self.focus_mode != FocusMode::Browse {
            return;
        }
        self.focus_mode = FocusMode::Input;
        self.clear_selection();
    }

    /// `k`/`↑` and `j`/`↓`: move the logical cursor by one wrapped row and
    /// chase it with the viewport.
    fn browse_step(&mut self, delta: isize) -> Vec<Effect> {
        let width = self.layout_width.get();
        let height = self.layout_history_height.get();
        self.cells
            .refresh_wraps(width, &|index| self.collapser(index));
        let wrapped = self.cells.wrap_rows();
        if wrapped.is_empty() {
            return vec![];
        }
        let next = move_cursor(&wrapped, self.browse_cursor, delta);
        drop(wrapped);
        self.browse_cursor = next;
        self.scroll_cursor_into_view(height);
        vec![]
    }

    /// `PgUp` / `PgDn`: page the cursor by a whole viewport (页 = 视口高 - 2).
    /// The viewport ride reuses the page-scroll engine; the cursor follows
    /// on the residual half-page and the view chase clamps the edges.
    fn browse_page(&mut self, delta: isize) -> Vec<Effect> {
        let width = self.layout_width.get();
        let height = self.layout_history_height.get();
        if delta < 0 {
            self.page_transcript_up(width, height);
        } else {
            self.page_transcript_down(width, height);
        }
        let page = isize::try_from(height.saturating_sub(2).max(1)).unwrap_or(1);
        self.browse_step(delta * page)
    }

    /// `Space` / `o`: toggle the foldable element under the cursor — a
    /// think header, or any body row of a foldable tool card. Rows that are
    /// neither leave the transcript untouched.
    fn browse_toggle_fold(&mut self) -> Vec<Effect> {
        let (cell, row) = self.browse_cursor;
        let toggled = if self.is_think_head(cell, row) {
            self.toggle_reasoning_block(cell, row)
        } else if self.is_tool_card_body(cell, row) {
            self.toggle_tool_card_fold(cell)
        } else {
            false
        };
        if toggled {
            self.rewrap_cursor();
        }
        vec![]
    }

    /// `Home` in browse mode: jump the viewport and cursor to the head.
    fn browse_home(&mut self) -> Vec<Effect> {
        self.jump_transcript_top();
        self.browse_cursor = (0, 0);
        vec![]
    }

    /// `End` in browse mode: jump the viewport and cursor to the tail.
    fn browse_end(&mut self) -> Vec<Effect> {
        self.jump_transcript_bottom();
        let width = self.layout_width.get();
        self.cells
            .refresh_wraps(width, &|index| self.collapser(index));
        let wrapped = self.cells.wrap_rows();
        self.browse_cursor = if wrapped.is_empty() {
            (0, 0)
        } else {
            let last = wrapped.len() - 1;
            (last, wrapped[last].len().saturating_sub(1))
        };
        vec![]
    }

    /// Whether the cursor sits on a reasoning-run head row (row 0 of the
    /// run's first cell) — the think block Space toggles.
    fn is_think_head(&self, cell: usize, row: usize) -> bool {
        if row != 0 || cell >= self.cells.display_len() {
            return false;
        }
        if self.cells.display_kind(cell) != LineKind::Reasoning {
            return false;
        }
        cell == 0 || self.cells.display_kind(cell - 1) != LineKind::Reasoning
    }

    /// Whether the cursor sits on a body row of a foldable tool card (any
    /// row past its header), so Space may fold or unfold the body.
    fn is_tool_card_body(&self, cell: usize, row: usize) -> bool {
        let Some(line) = self.cells.cell_at(cell) else {
            return false;
        };
        if line.kind != LineKind::Tool {
            return false;
        }
        let Some(body) = &line.body else {
            return false;
        };
        if body.lines.len() <= body.threshold {
            return false;
        }
        if line.header.is_some() {
            row > 0
        } else {
            true
        }
    }

    /// A fold toggle just changed the target cell's row count: re-clamp the
    /// cursor onto the rebuilt wraps and keep it inside the viewport.
    fn rewrap_cursor(&mut self) {
        let width = self.layout_width.get();
        let height = self.layout_history_height.get();
        self.cells
            .refresh_wraps(width, &|index| self.collapser(index));
        let wrapped = self.cells.wrap_rows();
        self.browse_cursor = clamp_cursor(&wrapped, self.browse_cursor);
        drop(wrapped);
        self.scroll_cursor_into_view(height);
    }

    /// Chases the viewport so the browse cursor row stays visible: pinned
    /// to the top when the cursor steps above it, to the bottom when it
    /// steps past it.
    fn scroll_cursor_into_view(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        let width = self.layout_width.get();
        self.cells
            .refresh_wraps(width, &|index| self.collapser(index));
        let wrapped = self.cells.wrap_rows();
        if wrapped.is_empty() {
            return;
        }
        let cursor_index = logical_index(&wrapped, self.browse_cursor);
        let view_top = self.scroll.scroll_offset(&wrapped, height);
        if cursor_index < view_top {
            let target = position_at_index(&wrapped, cursor_index);
            self.scroll.pin_at(&wrapped, height, target);
        } else if cursor_index >= view_top + height {
            let target = position_at_index(&wrapped, cursor_index.saturating_sub(height - 1));
            self.scroll.pin_at(&wrapped, height, target);
        }
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

    /// Whether any run phase currently owns the state badge (P2 footer):
    /// the composer prompt dims while a turn is on the wire.
    pub(crate) fn run_state_active(&self) -> bool {
        self.run_state.is_active()
    }

    #[cfg(test)]
    pub(crate) fn run_state_mut(&mut self) -> &mut RunState {
        &mut self.run_state
    }

    pub(crate) fn has_overlay(&self) -> bool {
        self.confirm.is_some() || self.picker.is_some()
    }

    pub(crate) fn input_focused(&self) -> bool {
        self.confirm.is_none()
            && self.picker.is_none()
            && self.focus_mode == FocusMode::Input
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
        // P4 history browse mode: below the overlays and the command menu,
        // above the composer's input dispatch.
        if self.focus_mode == FocusMode::Browse {
            return self.on_browse_action(action);
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
            Action::EnterBrowse => {
                self.enter_browse();
                vec![]
            }
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
            // Browse-mode keys never reach the composer: the keymap only
            // produces them while browse mode owns the focus, and a stray
            // programmatic dispatch is inert here.
            Action::BrowseStep(_) | Action::BrowsePage(_) | Action::BrowseToggleFold
            | Action::ExitBrowse => vec![],
            Action::None => vec![],
        }
    }

    /// P4 history browse mode (P5 §1): keyboard dispatch while the focus
    /// sits on the transcript. The composer, overlays and the command menu
    /// are all below this layer. Mouse events pass straight through — wheel
    /// scrolls, click-select and think-header clicks keep working.
    fn on_browse_action(&mut self, action: Action) -> Vec<Effect> {
        // Any interaction other than Ctrl+C disarms the two-step exit, so
        // browsing history never counts toward a quit chord.
        if !matches!(action, Action::CtrlC) {
            self.exit_armed = false;
        }
        match action {
            Action::BrowseStep(delta) => self.browse_step(delta),
            Action::BrowsePage(delta) => self.browse_page(delta),
            Action::BrowseToggleFold => self.browse_toggle_fold(),
            Action::Home => self.browse_home(),
            Action::End => self.browse_end(),
            Action::ExitBrowse | Action::Escape => {
                self.exit_browse();
                vec![]
            }
            Action::Submit => self.submit(),
            Action::CtrlC => {
                let effects = self.ctrl_c();
                self.exit_browse();
                effects
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
            Action::None => vec![],
            _ => vec![],
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
        header: None,
        body: None,
    }
}
