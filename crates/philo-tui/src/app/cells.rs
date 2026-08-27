//! Ordered transcript cells with an in-place open cursor, and the wrap slice.
//!
//! `TranscriptLine` is the Step-1 cell. Wrapped rows are derived at the
//! current width and are never the source of truth. In-progress Answer/Think
//! is a real cell at its insertion point (`open` is always the last index).
//! Wrap rows are cached per width: the closed prefix is stable; a width
//! change rebuilds everything; the open cell is always rewrapped.
//!
//! The store also owns think-block timing (design §3.2): each reasoning
//! block's header displays the wall clock from its first to its latest
//! `ReasoningDelta`, ticking live while the block streams and freezing at
//! its seal. Replay never records timing, so replayed headers stay bare.

use std::cell::{Ref, RefCell};
use std::time::{Duration, Instant};

use super::prose::{self, ProjectedRow};
use super::text;
use super::transcript::{LineKind, TranscriptLine};

#[derive(Clone, Debug)]
struct WrapCache {
    width: usize,
    closed_upto: usize,
    revision: u64,
    rows: Vec<Vec<ProjectedRow>>,
}

impl Default for WrapCache {
    fn default() -> Self {
        Self {
            width: usize::MAX,
            closed_upto: 0,
            revision: 0,
            rows: Vec::new(),
        }
    }
}

impl WrapCache {
    fn invalidate(&mut self) {
        *self = Self::default();
    }
}

/// Whether each reasoning run renders collapsed. Keyed by the run's first
/// cell index; other indices are never queried.
pub(crate) type Collapser<'a> = &'a dyn Fn(usize) -> bool;

#[cfg(test)]
pub(crate) fn expanded() -> Collapser<'static> {
    &|_| false
}

/// Wall-clock timing for one think block, keyed by its header cell index
/// (design §3.2). `live` blocks tick against the real clock at projection
/// time; sealed blocks show the captured first-to-last delta span.
#[derive(Clone, Debug)]
struct ThinkTiming {
    head: usize,
    started_at: Instant,
    /// Frozen first-to-last delta span once the block stops streaming.
    elapsed: Duration,
    live: bool,
    #[cfg(test)]
    frozen: Option<Duration>,
}

impl ThinkTiming {
    fn elapsed(&self) -> Duration {
        #[cfg(test)]
        if let Some(frozen) = self.frozen {
            return frozen;
        }
        if self.live {
            self.started_at.elapsed()
        } else {
            self.elapsed
        }
    }
}

/// Canonical history for one TUI session: one ordered cell list plus an
/// optional in-place open cursor on the last cell.
#[derive(Clone, Debug, Default)]
pub(crate) struct TranscriptStore {
    cells: Vec<TranscriptLine>,
    open: Option<usize>,
    wrap_revision: u64,
    cache: RefCell<WrapCache>,
    think: Vec<ThinkTiming>,
}

impl TranscriptStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.cells.clear();
        self.open = None;
        self.think.clear();
        self.wrap_revision = self.wrap_revision.wrapping_add(1);
        self.cache.borrow_mut().invalidate();
    }

    /// Manual reasoning fold changes must rebuild cached rows.
    pub(crate) fn bump_wrap_revision(&mut self) {
        self.wrap_revision = self.wrap_revision.wrapping_add(1);
    }

    // -- Think timing (design §3.2) --------------------------------------

    /// A reasoning block opened its header at `head`: start its wall clock.
    pub(crate) fn begin_think(&mut self, head: usize) {
        self.think.retain(|timing| timing.head != head);
        self.think.push(ThinkTiming {
            head,
            started_at: Instant::now(),
            elapsed: Duration::ZERO,
            live: true,
            #[cfg(test)]
            frozen: None,
        });
    }

    /// The live block received another delta: extend its frozen span.
    pub(crate) fn extend_think(&mut self) {
        if let Some(timing) = self.think.iter_mut().find(|timing| timing.live) {
            timing.elapsed = timing.started_at.elapsed();
        }
    }

    /// Streaming ended for every live block: freeze their spans and drop
    /// the cached rows so the next projection bakes the final durations in.
    pub(crate) fn seal_think(&mut self) {
        let mut sealed = false;
        for timing in &mut self.think {
            if timing.live {
                timing.elapsed = timing.started_at.elapsed();
                timing.live = false;
                sealed = true;
            }
        }
        if sealed {
            self.bump_wrap_revision();
        }
    }

    /// Display duration of the think block whose header sits at `head`.
    fn think_elapsed_at(&self, head: usize) -> Option<Duration> {
        self.think
            .iter()
            .find(|timing| timing.head == head)
            .map(ThinkTiming::elapsed)
    }

    /// Pins every think duration so snapshots stay deterministic (plan T3.4
    /// clock discipline, applied to the transcript).
    #[cfg(test)]
    pub(crate) fn freeze_think(&mut self, elapsed: Duration) {
        for timing in &mut self.think {
            timing.frozen = Some(elapsed);
        }
    }

    /// All cells, including the open cell when one exists.
    #[cfg(test)]
    pub fn cells(&self) -> &[TranscriptLine] {
        &self.cells
    }

    /// Same ordered list as the store cells (including the open cell).
    pub fn display_cells(&self) -> Vec<TranscriptLine> {
        self.cells.clone()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn display_len(&self) -> usize {
        self.cells.len()
    }

    pub fn display_kind(&self, index: usize) -> LineKind {
        self.cells[index].kind
    }

    /// Paint intent of the cell at `index` (tool-card structure, §3.3).
    pub(crate) fn display_tone(&self, index: usize) -> super::transcript::Tone {
        self.cells[index].tone
    }

    #[cfg(test)]
    pub fn open_index(&self) -> Option<usize> {
        self.open
    }

    #[cfg(test)]
    pub fn has_open(&self) -> bool {
        self.open.is_some()
    }

    pub(crate) fn open_kind(&self) -> Option<LineKind> {
        self.open.map(|idx| self.cells[idx].kind)
    }

    /// Close any open cell, then append already-finished cells.
    pub fn push_closed(&mut self, lines: impl IntoIterator<Item = TranscriptLine>) {
        self.close_open();
        self.cells.extend(lines);
    }

    /// Close any open cell, then push a new last cell and mark it open.
    pub fn begin(&mut self, kind: LineKind, text: impl Into<String>) {
        self.close_open();
        self.cells.push(TranscriptLine {
            kind,
            text: text.into(),
            tone: super::transcript::Tone::Plain,
        });
        self.open = Some(self.cells.len() - 1);
        self.assert_open_last();
    }

    /// Append to the open cell. Panics if nothing is open.
    pub fn write_open(&mut self, text: &str) {
        let idx = self.open.expect("write_open requires an open cell");
        debug_assert_eq!(
            idx,
            self.cells.len() - 1,
            "open cursor must be the last cell"
        );
        self.cells[idx].text.push_str(text);
    }

    pub fn close_open(&mut self) {
        self.open = None;
    }

    /// Remove the open cell so a line-oriented think remainder can be rewritten.
    pub(crate) fn take_open(&mut self) -> Option<TranscriptLine> {
        let idx = self.open.take()?;
        debug_assert_eq!(
            idx,
            self.cells.len() - 1,
            "open cursor must be the last cell"
        );
        self.cells.pop()
    }

    fn assert_open_last(&self) {
        debug_assert!(
            self.open.is_none_or(|idx| idx + 1 == self.cells.len()),
            "open cursor must be the last cell"
        );
    }

    /// Rebuild wrap rows when the width changes or a manual reasoning fold
    /// bumped the revision. The closed prefix is stable; always rewrap from
    /// the open cell — or, while a think block streams, from its header so
    /// the live duration re-renders every frame (design §3.2).
    pub(crate) fn refresh_wraps(&self, width: usize, collapsed: Collapser<'_>) {
        self.assert_open_last();
        let mut cache = self.cache.borrow_mut();
        if cache.revision != self.wrap_revision {
            *cache = WrapCache {
                width,
                revision: self.wrap_revision,
                ..WrapCache::default()
            };
        }
        if width == 0 {
            cache.invalidate();
            cache.width = 0;
            cache.revision = self.wrap_revision;
            return;
        }
        if cache.width != width {
            cache.invalidate();
            cache.width = width;
            cache.revision = self.wrap_revision;
        }
        let rewrap_from = match (self.open, self.live_think_head()) {
            (Some(open), Some(head)) => open.min(head),
            (Some(open), None) => open,
            (None, Some(head)) => head,
            (None, None) => self.cells.len(),
        }
        .min(self.cells.len());
        if cache.closed_upto > rewrap_from {
            cache.rows.truncate(rewrap_from);
            cache.closed_upto = rewrap_from;
        }
        if cache.closed_upto > self.cells.len() {
            cache.rows.truncate(self.cells.len());
            cache.closed_upto = self.cells.len();
        }
        let runs = ReasoningRuns::scan(&self.cells);
        let start = cache.closed_upto.min(rewrap_from);
        cache.rows.truncate(start);
        cache.closed_upto = start;
        for index in start..rewrap_from {
            let row = project_cell(
                &self.cells[index],
                width,
                self.prev_kind(index),
                &runs,
                index,
                collapsed,
                self.think_elapsed_at(index),
            );
            cache.rows.push(row);
            cache.closed_upto += 1;
        }
        for index in rewrap_from..self.cells.len() {
            let row = project_cell(
                &self.cells[index],
                width,
                self.prev_kind(index),
                &runs,
                index,
                collapsed,
                self.think_elapsed_at(index),
            );
            cache.rows.push(row);
        }
    }

    fn live_think_head(&self) -> Option<usize> {
        self.think
            .iter()
            .find(|timing| timing.live)
            .map(|timing| timing.head)
    }

    fn prev_kind(&self, index: usize) -> Option<LineKind> {
        index.checked_sub(1).map(|prev| self.cells[prev].kind)
    }

    pub(crate) fn wrap_rows(&self) -> Ref<'_, [Vec<ProjectedRow>]> {
        Ref::map(self.cache.borrow(), |cache| cache.rows.as_slice())
    }

    pub(crate) fn visible_slice(
        &self,
        width: usize,
        height: usize,
        scroll: &ScrollState,
        collapsed: Collapser<'_>,
    ) -> VisibleSlice {
        self.refresh_wraps(width, collapsed);
        let wrapped = self.wrap_rows();
        visible_from_wraps(self, &wrapped, width, height, scroll)
    }

    #[cfg(test)]
    fn cached_width(&self) -> usize {
        self.cache.borrow().width
    }

    #[cfg(test)]
    fn cached_closed_upto(&self) -> usize {
        self.cache.borrow().closed_upto
    }
}

/// Follow-bottom is the only public scroll policy. `pin` is the first
/// visible `(cell_index, row_in_cell)` while the user has scrolled up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScrollState {
    follow_bottom: bool,
    pin: Option<(usize, usize)>,
}

impl ScrollState {
    pub(crate) fn follow() -> Self {
        Self {
            follow_bottom: true,
            pin: None,
        }
    }

    pub(crate) fn follow_bottom(&self) -> bool {
        self.follow_bottom
    }

    /// `delta < 0` moves toward older rows. `height` is the transcript
    /// region, not the full terminal.
    #[cfg(test)]
    pub(crate) fn scroll_rows(
        &mut self,
        cells: &[TranscriptLine],
        width: usize,
        height: usize,
        delta: isize,
    ) {
        if delta == 0 || width == 0 || height == 0 || cells.is_empty() {
            return;
        }
        let wrapped = wrap_all(cells, width);
        self.scroll_wrapped(&wrapped, height, delta);
    }

    pub(crate) fn scroll_wrapped(
        &mut self,
        wrapped: &[Vec<ProjectedRow>],
        height: usize,
        delta: isize,
    ) {
        if delta == 0 || height == 0 || wrapped.is_empty() {
            return;
        }
        if row_count(wrapped) <= height {
            *self = Self::follow();
            return;
        }

        let (mut cell, mut row) = if self.follow_bottom {
            start_from_tail(wrapped, height)
        } else {
            clamp_pin(self.pin, wrapped)
        };

        if delta < 0 {
            move_backward(wrapped, &mut cell, &mut row, delta.unsigned_abs());
            self.follow_bottom = false;
            self.pin = Some((cell, row));
            return;
        }

        move_forward(wrapped, &mut cell, &mut row, delta as usize);
        if window_reaches_tail(wrapped, cell, row, height) {
            *self = Self::follow();
        } else {
            self.follow_bottom = false;
            self.pin = Some((cell, row));
        }
    }

    /// Stop following without moving the current visible window.
    #[cfg(test)]
    pub(crate) fn unfollow_keep_view(
        &mut self,
        cells: &[TranscriptLine],
        width: usize,
        height: usize,
    ) {
        if !self.follow_bottom {
            return;
        }
        if width == 0 || height == 0 || cells.is_empty() {
            self.follow_bottom = false;
            self.pin = Some((0, 0));
            return;
        }
        let wrapped = wrap_all(cells, width);
        self.unfollow_keep_wrapped(&wrapped, height);
    }

    pub(crate) fn unfollow_keep_wrapped(&mut self, wrapped: &[Vec<ProjectedRow>], height: usize) {
        if !self.follow_bottom {
            return;
        }
        if height == 0 || wrapped.is_empty() {
            self.follow_bottom = false;
            self.pin = Some((0, 0));
            return;
        }
        self.follow_bottom = false;
        self.pin = Some(start_from_tail(wrapped, height));
    }

    pub(crate) fn jump_top(&mut self, wrapped: &[Vec<ProjectedRow>], height: usize) {
        if height == 0 || wrapped.is_empty() || row_count(wrapped) <= height {
            *self = Self::follow();
            return;
        }
        self.follow_bottom = false;
        self.pin = Some((0, 0));
    }

    pub(crate) fn jump_bottom(&mut self) {
        *self = Self::follow();
    }
}

impl Default for ScrollState {
    fn default() -> Self {
        Self::follow()
    }
}

/// One display row after wrapping a sealed cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VisibleRow {
    pub cell_index: usize,
    pub row_in_cell: usize,
    pub kind: LineKind,
    pub tone: super::transcript::Tone,
    /// Block role of the answer line this fragment came from (`Plain`
    /// outside answer cells; see [`super::prose`]).
    pub role: super::prose::BlockRole,
    /// Baked presentation spans for answer prose rows (`None` elsewhere
    /// and for fenced code bodies).
    pub spans: Option<Vec<super::prose::ProseSpan>>,
    pub text: String,
}

/// Visible transcript window at a given width and height.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VisibleSlice {
    pub rows: Vec<VisibleRow>,
    pub total_rows: usize,
    pub follow_bottom: bool,
    pub at_top: bool,
    pub at_bottom: bool,
}

/// Pure. No Ratatui. No markdown painting.
/// `width == 0` or `height == 0` yields empty rows; `total_rows` is still
/// computed when `width > 0`.
#[cfg(test)]
pub(crate) fn visible_slice(
    cells: &[TranscriptLine],
    width: usize,
    height: usize,
    scroll: &ScrollState,
) -> VisibleSlice {
    if width == 0 {
        return VisibleSlice {
            rows: Vec::new(),
            total_rows: 0,
            follow_bottom: scroll.follow_bottom(),
            at_top: true,
            at_bottom: true,
        };
    }

    let wrapped = wrap_all(cells, width);
    slice_from_wraps(cells, &wrapped, width, height, scroll)
}

fn visible_from_wraps(
    store: &TranscriptStore,
    wrapped: &[Vec<ProjectedRow>],
    width: usize,
    height: usize,
    scroll: &ScrollState,
) -> VisibleSlice {
    if width == 0 {
        return VisibleSlice {
            rows: Vec::new(),
            total_rows: 0,
            follow_bottom: scroll.follow_bottom(),
            at_top: true,
            at_bottom: true,
        };
    }
    let total_rows = row_count(wrapped);
    if height == 0 {
        return VisibleSlice {
            rows: Vec::new(),
            total_rows,
            follow_bottom: scroll.follow_bottom(),
            at_top: true,
            at_bottom: true,
        };
    }
    let (start_cell, start_row) = if scroll.follow_bottom() {
        start_from_tail(wrapped, height)
    } else {
        clamp_pin(scroll.pin, wrapped)
    };
    let rows = collect_store_rows(store, wrapped, start_cell, start_row, height);
    let at_top = start_cell == 0 && start_row == 0;
    let at_bottom = window_reaches_tail(wrapped, start_cell, start_row, height);
    VisibleSlice {
        rows,
        total_rows,
        follow_bottom: scroll.follow_bottom(),
        at_top,
        at_bottom,
    }
}

#[cfg(test)]
fn slice_from_wraps(
    cells: &[TranscriptLine],
    wrapped: &[Vec<ProjectedRow>],
    width: usize,
    height: usize,
    scroll: &ScrollState,
) -> VisibleSlice {
    if width == 0 {
        return VisibleSlice {
            rows: Vec::new(),
            total_rows: 0,
            follow_bottom: scroll.follow_bottom(),
            at_top: true,
            at_bottom: true,
        };
    }
    let total_rows = row_count(wrapped);
    if height == 0 {
        return VisibleSlice {
            rows: Vec::new(),
            total_rows,
            follow_bottom: scroll.follow_bottom(),
            at_top: true,
            at_bottom: true,
        };
    }
    let (start_cell, start_row) = if scroll.follow_bottom() {
        start_from_tail(wrapped, height)
    } else {
        clamp_pin(scroll.pin, wrapped)
    };
    let rows = collect_rows(cells, wrapped, start_cell, start_row, height);
    let at_top = start_cell == 0 && start_row == 0;
    let at_bottom = window_reaches_tail(wrapped, start_cell, start_row, height);
    VisibleSlice {
        rows,
        total_rows,
        follow_bottom: scroll.follow_bottom(),
        at_top,
        at_bottom,
    }
}

pub(crate) fn wrap_line(
    cell: &TranscriptLine,
    width: usize,
    prev: Option<LineKind>,
) -> Vec<ProjectedRow> {
    let mut rows = match cell.kind {
        // User rows paint one space ahead of the text (the strip's bar-gap
        // rhythm); reserve that column in the wrap width (v2.3).
        LineKind::User => plain_rows(text::wrap(&cell.text, width.saturating_sub(1).max(1))),
        LineKind::Answer => prose::project_answer(&cell.text, width),
        LineKind::Tool => plain_rows(text::wrap_hanging(&cell.text, width)),
        LineKind::Reasoning => plain_rows(text::wrap_reasoning(&cell.text, width)),
        _ => plain_rows(text::wrap(&cell.text, width)),
    };
    if needs_leading_gap(prev, cell) && !rows.is_empty() {
        rows.insert(0, ProjectedRow::plain(String::new()));
    }
    rows
}

fn plain_rows(rows: impl IntoIterator<Item = String>) -> Vec<ProjectedRow> {
    rows.into_iter().map(ProjectedRow::plain).collect()
}

/// Consecutive Reasoning cells form one visual think block. The scan maps
/// every reasoning index to its run head; the header's duration comes from
/// the store's think timing, not the body shape.
struct ReasoningRuns {
    head_of: Vec<usize>,
}

impl ReasoningRuns {
    fn scan(cells: &[TranscriptLine]) -> Self {
        let mut head_of = vec![usize::MAX; cells.len()];
        let mut index = 0;
        while index < cells.len() {
            if cells[index].kind != LineKind::Reasoning {
                index += 1;
                continue;
            }
            let head = index;
            while index < cells.len() && cells[index].kind == LineKind::Reasoning {
                head_of[index] = head;
                index += 1;
            }
        }
        Self { head_of }
    }
}

/// The rendered header row of the think block whose header cell is `index`:
/// `think` bare (replay, or a block that never streamed deltas) or
/// `think · 8s` with the block's wall-clock span. Sub-second spans wear
/// milliseconds (`think · 850ms`, v2.2) instead of collapsing to `0s`.
fn think_header_text(elapsed: Option<Duration>) -> String {
    match elapsed {
        Some(elapsed) => {
            format!(
                "think · {}",
                crate::app::run_state::format_think_elapsed(elapsed)
            )
        }
        None => "think".to_owned(),
    }
}

/// One display row list for a cell: normal wrap, the collapsed `think`
/// header with hidden body when its run is folded, or the timed header of
/// an open run.
fn project_cell(
    cell: &TranscriptLine,
    width: usize,
    prev: Option<LineKind>,
    runs: &ReasoningRuns,
    index: usize,
    collapsed: Collapser<'_>,
    think_elapsed: Option<Duration>,
) -> Vec<ProjectedRow> {
    if cell.kind == LineKind::Reasoning && cell.text == "think" {
        let header = think_header_text(think_elapsed);
        let head = runs.head_of[index];
        if head != usize::MAX && collapsed(head) {
            return vec![ProjectedRow::plain(header)];
        }
        return plain_rows(text::wrap(&header, width));
    }
    if cell.kind == LineKind::Reasoning {
        let head = runs.head_of[index];
        if head != usize::MAX && collapsed(head) {
            return Vec::new();
        }
    }
    wrap_line(cell, width, prev)
}

/// Blank rows separate visual blocks. Cards own their inner spacing
/// explicitly, so plain tool rows stay gap-free; only a new card header
/// (`Tone::Title`) opens a gap after previous content — except after user
/// strips, which already carry their own trailing separator.
fn needs_leading_gap(prev: Option<LineKind>, cell: &TranscriptLine) -> bool {
    match cell.kind {
        LineKind::Tool => {
            cell.tone == super::transcript::Tone::Title
                && !matches!(prev, None | Some(LineKind::User))
        }
        LineKind::Answer => matches!(prev, Some(LineKind::Tool | LineKind::Answer)),
        _ => false,
    }
}

#[cfg(test)]
fn wrap_all(cells: &[TranscriptLine], width: usize) -> Vec<Vec<ProjectedRow>> {
    cells
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            let prev = if index == 0 {
                None
            } else {
                Some(cells[index - 1].kind)
            };
            wrap_line(cell, width, prev)
        })
        .collect()
}

fn row_count(wrapped: &[Vec<ProjectedRow>]) -> usize {
    wrapped.iter().map(Vec::len).sum()
}

fn start_from_tail(wrapped: &[Vec<ProjectedRow>], height: usize) -> (usize, usize) {
    if wrapped.is_empty() {
        return (0, 0);
    }
    let mut remaining = height;
    for (cell_idx, rows) in wrapped.iter().enumerate().rev() {
        if rows.len() >= remaining {
            return (cell_idx, rows.len() - remaining);
        }
        remaining -= rows.len();
    }
    (0, 0)
}

fn clamp_pin(pin: Option<(usize, usize)>, wrapped: &[Vec<ProjectedRow>]) -> (usize, usize) {
    let Some((cell, row)) = pin else {
        return (0, 0);
    };
    if wrapped.is_empty() {
        return (0, 0);
    }
    let cell = cell.min(wrapped.len() - 1);
    let len = wrapped[cell].len();
    let row = if len == 0 { 0 } else { row.min(len - 1) };
    (cell, row)
}

fn rows_from(wrapped: &[Vec<ProjectedRow>], cell: usize, row: usize) -> usize {
    if cell >= wrapped.len() {
        return 0;
    }
    let first = wrapped[cell].len().saturating_sub(row);
    first + wrapped[cell + 1..].iter().map(Vec::len).sum::<usize>()
}

fn window_reaches_tail(
    wrapped: &[Vec<ProjectedRow>],
    cell: usize,
    row: usize,
    height: usize,
) -> bool {
    wrapped.is_empty() || rows_from(wrapped, cell, row) <= height
}

fn move_backward(wrapped: &[Vec<ProjectedRow>], cell: &mut usize, row: &mut usize, mut n: usize) {
    while n > 0 {
        if *row > 0 {
            let step = (*row).min(n);
            *row -= step;
            n -= step;
        } else if *cell > 0 {
            *cell -= 1;
            *row = wrapped[*cell].len().saturating_sub(1);
            n -= 1;
        } else {
            break;
        }
    }
}

fn move_forward(wrapped: &[Vec<ProjectedRow>], cell: &mut usize, row: &mut usize, mut n: usize) {
    while n > 0 {
        let len = wrapped.get(*cell).map(Vec::len).unwrap_or(0);
        if *row + 1 < len {
            let step = (len - 1 - *row).min(n);
            *row += step;
            n -= step;
        } else if *cell + 1 < wrapped.len() {
            *cell += 1;
            *row = 0;
            n -= 1;
        } else {
            break;
        }
    }
}

#[cfg(test)]
fn collect_rows(
    cells: &[TranscriptLine],
    wrapped: &[Vec<ProjectedRow>],
    start_cell: usize,
    start_row: usize,
    height: usize,
) -> Vec<VisibleRow> {
    let mut out = Vec::new();
    let mut cell = start_cell;
    let mut row = start_row;
    while out.len() < height && cell < cells.len() {
        let rows = &wrapped[cell];
        if row < rows.len() {
            out.push(VisibleRow {
                cell_index: cell,
                row_in_cell: row,
                kind: cells[cell].kind,
                tone: cells[cell].tone,
                role: rows[row].role.clone(),
                spans: rows[row].spans.clone(),
                text: rows[row].text.clone(),
            });
            row += 1;
        } else {
            cell += 1;
            row = 0;
        }
    }
    out
}

fn collect_store_rows(
    store: &TranscriptStore,
    wrapped: &[Vec<ProjectedRow>],
    start_cell: usize,
    start_row: usize,
    height: usize,
) -> Vec<VisibleRow> {
    let mut out = Vec::new();
    let mut cell = start_cell;
    let mut row = start_row;
    let len = store.display_len().min(wrapped.len());
    while out.len() < height && cell < len {
        let rows = &wrapped[cell];
        if row < rows.len() {
            out.push(VisibleRow {
                cell_index: cell,
                row_in_cell: row,
                kind: store.display_kind(cell),
                tone: store.display_tone(cell),
                role: rows[row].role.clone(),
                spans: rows[row].spans.clone(),
                text: rows[row].text.clone(),
            });
            row += 1;
        } else {
            cell += 1;
            row = 0;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::transcript::{Tone, line};

    fn meta(text: &str) -> TranscriptLine {
        line(LineKind::Meta, text)
    }

    fn texts(slice: &VisibleSlice) -> Vec<&str> {
        slice.rows.iter().map(|row| row.text.as_str()).collect()
    }

    fn row_texts(rows: &[ProjectedRow]) -> Vec<&str> {
        rows.iter().map(|row| row.text.as_str()).collect()
    }

    #[test]
    fn follow_shows_the_tail() {
        let cells: Vec<_> = (0..10).map(|i| meta(&format!("row-{i}"))).collect();
        let scroll = ScrollState::follow();
        let slice = visible_slice(&cells, 80, 3, &scroll);
        assert_eq!(texts(&slice), ["row-7", "row-8", "row-9"]);
        assert_eq!(slice.total_rows, 10);
        assert!(slice.follow_bottom);
        assert!(slice.at_bottom);
        assert!(!slice.at_top);
    }

    #[test]
    fn unfollow_keep_view_pins_the_current_tail_window() {
        let cells: Vec<_> = (0..10).map(|i| meta(&format!("row-{i}"))).collect();
        let mut scroll = ScrollState::follow();
        scroll.unfollow_keep_view(&cells, 80, 3);
        assert!(!scroll.follow_bottom());
        let slice = visible_slice(&cells, 80, 3, &scroll);
        assert_eq!(texts(&slice), ["row-7", "row-8", "row-9"]);
        assert!(!slice.follow_bottom);
    }

    #[test]
    fn page_up_then_append_does_not_yank() {
        let mut cells: Vec<_> = (0..10).map(|i| meta(&format!("row-{i}"))).collect();
        let mut scroll = ScrollState::follow();
        scroll.scroll_rows(&cells, 80, 3, -3);
        let before = visible_slice(&cells, 80, 3, &scroll);
        assert_eq!(texts(&before), ["row-4", "row-5", "row-6"]);
        assert!(!scroll.follow_bottom());

        cells.push(meta("row-10"));
        cells.push(meta("row-11"));
        let after = visible_slice(&cells, 80, 3, &scroll);
        assert_eq!(texts(&after), ["row-4", "row-5", "row-6"]);
        assert!(!scroll.follow_bottom());
        assert!(!after.at_bottom);
    }

    #[test]
    fn page_down_to_tail_restores_follow() {
        let cells: Vec<_> = (0..10).map(|i| meta(&format!("row-{i}"))).collect();
        let mut scroll = ScrollState::follow();
        scroll.scroll_rows(&cells, 80, 3, -3);
        assert!(!scroll.follow_bottom());
        scroll.scroll_rows(&cells, 80, 3, 3);
        scroll.scroll_rows(&cells, 80, 3, 3);
        assert!(scroll.follow_bottom());
        let slice = visible_slice(&cells, 80, 3, &scroll);
        assert_eq!(texts(&slice), ["row-7", "row-8", "row-9"]);
        assert!(slice.at_bottom);
    }

    #[test]
    fn zero_width_or_height_yields_empty_rows() {
        let cells = vec![meta("hello")];
        let scroll = ScrollState::follow();
        let no_width = visible_slice(&cells, 0, 3, &scroll);
        assert!(no_width.rows.is_empty());
        assert_eq!(no_width.total_rows, 0);

        let no_height = visible_slice(&cells, 80, 0, &scroll);
        assert!(no_height.rows.is_empty());
        assert_eq!(no_height.total_rows, 1);
    }

    #[test]
    fn wrap_uses_cjk_cell_width() {
        let cells = vec![meta("中文ab")];
        let scroll = ScrollState::follow();
        let slice = visible_slice(&cells, 4, 2, &scroll);
        assert_eq!(texts(&slice), ["中文", "ab"]);
        assert_eq!(slice.rows[0].row_in_cell, 0);
        assert_eq!(slice.rows[1].row_in_cell, 1);
        assert_eq!(slice.total_rows, 2);
    }

    #[test]
    fn user_rows_wrap_one_short_reserving_the_bar_gap() {
        let cells = vec![line(LineKind::User, "abcdefgh")];
        let scroll = ScrollState::follow();
        // Width 6 reserves one column for the painted bar gap, so the text
        // wraps at 5.
        let slice = visible_slice(&cells, 6, 2, &scroll);
        assert_eq!(texts(&slice), ["abcde", "fgh"]);
    }

    #[test]
    fn block_boundaries_get_a_leading_gap_row() {
        let tool = |text: &str, tone: Tone| crate::app::transcript::TranscriptLine {
            kind: LineKind::Tool,
            text: text.to_owned(),
            tone,
        };
        let cells = vec![
            line(LineKind::Answer, "abcdefgh"),
            line(LineKind::Answer, "ijklmnop"),
            tool("Grep 1 search", Tone::Title),
            tool("  ↳ \"hit\"", Tone::Detail),
            line(LineKind::Answer, "next"),
        ];
        let scroll = ScrollState::follow();
        let slice = visible_slice(&cells, 6, 12, &scroll);
        assert_eq!(
            texts(&slice),
            [
                "gh",
                "",
                "ijklmn",
                "op",
                "",
                "Grep 1",
                " searc",
                "h",
                "  ↳ \"h",
                "  it\"",
                "",
                "next"
            ]
        );
    }

    #[test]
    fn user_and_reasoning_boundaries_stay_gap_free() {
        let cells = vec![
            line(LineKind::User, "prompt"),
            line(LineKind::Answer, "reply"),
            line(LineKind::Reasoning, "think"),
            line(LineKind::Reasoning, "  more think"),
            line(LineKind::Meta, "note"),
        ];
        let scroll = ScrollState::follow();
        let slice = visible_slice(&cells, 20, 8, &scroll);
        assert_eq!(
            texts(&slice),
            ["prompt", "reply", "think", "│ more think", "note"]
        );
    }

    #[test]
    fn short_transcript_fits_and_stays_following() {
        let cells = vec![meta("one"), meta("two")];
        let mut scroll = ScrollState::follow();
        scroll.scroll_rows(&cells, 80, 5, -5);
        assert!(scroll.follow_bottom());
        let slice = visible_slice(&cells, 80, 5, &scroll);
        assert_eq!(texts(&slice), ["one", "two"]);
        assert!(slice.at_top);
        assert!(slice.at_bottom);
    }

    #[test]
    fn sealed_reasoning_runs_project_a_collapsed_header() {
        let mut store = TranscriptStore::default();
        store.push_closed([
            meta("a"),
            crate::app::transcript::line(LineKind::Reasoning, "think"),
            crate::app::transcript::line(LineKind::Reasoning, "  body one"),
            crate::app::transcript::line(LineKind::Reasoning, "  body two"),
            meta("tail"),
        ]);
        store.refresh_wraps(80, &|head| head == 1);
        let slice = store.visible_slice(80, 10, &ScrollState::follow(), &|head| head == 1);
        assert_eq!(
            texts(&slice),
            ["a", "think", "tail"],
            "the folded run renders one header row"
        );

        let mut store = TranscriptStore::default();
        store.push_closed([crate::app::transcript::line(LineKind::Reasoning, "think")]);
        store.refresh_wraps(80, &|_| true);
        let slice = store.visible_slice(80, 10, &ScrollState::follow(), &|_| true);
        assert_eq!(texts(&slice), ["think"]);
    }

    #[test]
    fn timed_think_headers_render_and_freeze_at_the_seal() {
        let mut store = TranscriptStore::default();
        store.push_closed([
            crate::app::transcript::line(LineKind::Reasoning, "think"),
            crate::app::transcript::line(LineKind::Reasoning, "  body"),
            meta("after"),
        ]);
        let head = 0;

        // Streaming: the header ticks live against the injected clock.
        store.begin_think(head);
        store.extend_think();
        store.freeze_think(std::time::Duration::from_secs(8));
        store.refresh_wraps(80, &|_| true);
        {
            let rows = store.wrap_rows();
            assert_eq!(row_texts(&rows[head]), ["think · 8s"]);
            assert_eq!(row_texts(&rows[2]), ["after"]);
        }

        // Sealed: the frozen span survives the cache rebuild.
        store.seal_think();
        store.freeze_think(std::time::Duration::from_secs(8));
        store.refresh_wraps(80, &|_| false);
        {
            let rows = store.wrap_rows();
            assert_eq!(row_texts(&rows[head]), ["think · 8s"]);
            assert_eq!(row_texts(&rows[1]), ["│ body"]);
        }

        // Untimed blocks (replay) keep the bare header.
        let mut replay = TranscriptStore::default();
        replay.push_closed([crate::app::transcript::line(LineKind::Reasoning, "think")]);
        replay.refresh_wraps(80, &|_| true);
        assert_eq!(row_texts(&replay.wrap_rows()[0]), ["think"]);
    }

    #[test]
    fn fold_toggles_rebuild_cached_rows() {
        let mut store = TranscriptStore::default();
        store.push_closed([crate::app::transcript::line(LineKind::Reasoning, "think")]);
        store.refresh_wraps(80, &|_| true);
        {
            let rows = store.wrap_rows();
            assert_eq!(row_texts(&rows[0]), ["think"]);
        }
        store.bump_wrap_revision();
        store.refresh_wraps(80, &|_| false);
        {
            let rows = store.wrap_rows();
            assert_eq!(rows[0], wrap_line(&store.cells()[0], 80, None));
        }
    }

    #[test]
    fn is_empty_is_false_when_only_an_open_cell_exists() {
        let mut store = TranscriptStore::new();
        assert!(store.is_empty());
        store.begin(LineKind::Meta, "partial");
        assert!(!store.is_empty());
        assert_eq!(store.cells(), [meta("partial")]);
        assert_eq!(store.open_index(), Some(0));
        assert!(store.has_open());
        assert_eq!(store.display_cells(), vec![meta("partial")]);
        store.close_open();
        store.push_closed([meta("sealed")]);
        assert_eq!(store.cells(), [meta("partial"), meta("sealed")]);
        assert_eq!(store.display_cells(), vec![meta("partial"), meta("sealed")]);
        assert!(!store.has_open());
    }

    #[test]
    fn open_cursor_is_always_the_last_cell() {
        let mut store = TranscriptStore::new();
        store.begin(LineKind::Answer, "a");
        assert_eq!(store.open_index(), Some(store.cells().len() - 1));
        store.write_open("b");
        assert_eq!(store.cells()[0].text, "ab");
        store.push_closed([meta("x")]);
        assert!(!store.has_open());
        assert_eq!(
            store.cells().last().map(|cell| cell.text.as_str()),
            Some("x")
        );
        store.begin(LineKind::Answer, "c");
        assert_eq!(store.open_index(), Some(store.cells().len() - 1));
        store.close_open();
        assert!(!store.has_open());
        store.clear();
        assert!(store.is_empty());
        assert!(!store.has_open());
    }

    #[test]
    fn wrap_cache_is_keyed_by_width_and_rewraps_from_open() {
        let mut store = TranscriptStore::new();
        store.push_closed((0..3).map(|i| meta(&format!("中文{i}"))));
        store.refresh_wraps(4, expanded());
        assert_eq!(store.cached_width(), 4);
        assert_eq!(store.cached_closed_upto(), 3);
        let narrow = store.wrap_rows().to_vec();
        assert_eq!(row_texts(&narrow[0]), ["中文", "0"]);

        store.refresh_wraps(4, expanded());
        assert_eq!(
            store.cached_closed_upto(),
            3,
            "same width reuses closed wraps"
        );
        assert_eq!(&*store.wrap_rows(), narrow.as_slice());

        store.refresh_wraps(80, expanded());
        assert_eq!(store.cached_width(), 80);
        assert_eq!(row_texts(&store.wrap_rows()[0]), ["中文0"]);

        store.refresh_wraps(4, expanded());
        assert_eq!(&*store.wrap_rows(), narrow.as_slice());

        store.push_closed([meta("中文3")]);
        store.refresh_wraps(4, expanded());
        assert_eq!(store.cached_closed_upto(), 4);
        assert_eq!(row_texts(&store.wrap_rows()[3]), ["中文", "3"]);

        store.begin(LineKind::Meta, "中文");
        store.refresh_wraps(4, expanded());
        assert_eq!(store.cached_closed_upto(), 4);
        assert_eq!(row_texts(&store.wrap_rows()[4]), ["中文"]);
        store.write_open("live");
        store.refresh_wraps(4, expanded());
        assert_eq!(
            store.cached_closed_upto(),
            4,
            "open never advances closed cache"
        );
        assert_eq!(row_texts(&store.wrap_rows()[4]), ["中文", "live"]);
    }
}
