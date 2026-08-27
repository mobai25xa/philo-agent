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

use super::prose::{self, ProjectedRow, ProseColor, ProseSpan, ProseStyle};
use super::text;
use super::transcript::{CardBody, CardHeader, LineKind, SegColor, SegSpan, TranscriptLine};

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

    /// Read access to one settled cell (fold-state queries, tests).
    pub(crate) fn cell_at(&self, index: usize) -> Option<&TranscriptLine> {
        self.cells.get(index)
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
            header: None,
            body: None,
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

    /// Replace one settled cell in place. Live tool cards settle in their
    /// own cell (v4.0 P3 §4.1), so the store rewrites it without re-appending.
    /// The wrap cache is truncated from that cell so only its tail rewraps;
    /// no revision bump, so unchanged cells keep their cached rows.
    pub(crate) fn replace_cell(&mut self, index: usize, line: TranscriptLine) {
        assert!(
            index < self.cells.len(),
            "replace_cell index {} out of bounds ({} cells)",
            index,
            self.cells.len()
        );
        assert!(
            self.open != Some(index),
            "replace_cell must not touch the open cell"
        );
        self.cells[index] = line;
        let mut cache = self.cache.borrow_mut();
        cache.rows.truncate(index);
        cache.closed_upto = cache.closed_upto.min(index);
        cache.revision = self.wrap_revision;
    }

    /// Replaces everything from `index` onward with `lines`. The live single
    /// card settles from its one-cell form into a multi-cell settled card,
    /// so the tail (which is exactly the live card while a batch runs) is
    /// swapped wholesale (v4.0 P3 §4.1).
    pub(crate) fn replace_tail(&mut self, index: usize, lines: Vec<TranscriptLine>) {
        assert!(
            index <= self.cells.len(),
            "replace_tail index {} out of bounds ({} cells)",
            index,
            self.cells.len()
        );
        assert!(
            self.open.is_none_or(|open| open < index),
            "replace_tail must not cover the open cell"
        );
        self.cells.truncate(index);
        self.cells.extend(lines);
        let mut cache = self.cache.borrow_mut();
        cache.rows.truncate(index);
        cache.closed_upto = cache.closed_upto.min(index);
        cache.revision = self.wrap_revision;
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

    /// The pinned viewport origin, if the user has scrolled off the tail.
    pub(crate) fn pin(&self) -> Option<(usize, usize)> {
        self.pin
    }

    /// Pins the viewport so `target` becomes its top-left row (browse-mode
    /// cursor chasing). Short transcripts collapse back to follow.
    pub(crate) fn pin_at(
        &mut self,
        wrapped: &[Vec<ProjectedRow>],
        height: usize,
        target: (usize, usize),
    ) {
        if height == 0 || wrapped.is_empty() || row_count(wrapped) <= height {
            *self = Self::follow();
            return;
        }
        self.follow_bottom = false;
        self.pin = Some(clamp_pin(Some(target), wrapped));
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

    /// Wrapped-row offset of the visible window's top (the scrollbar's S):
    /// 0 while following (pinned to the tail), otherwise the rows above
    /// the pinned start.
    pub(crate) fn scroll_offset(&self, wrapped: &[Vec<ProjectedRow>], height: usize) -> usize {
        let total = row_count(wrapped);
        let viewport = height.min(total);
        if self.follow_bottom || total <= viewport {
            return total.saturating_sub(viewport);
        }
        let (cell, row) = clamp_pin(self.pin, wrapped);
        total
            .saturating_sub(rows_from(wrapped, cell, row))
            .min(total.saturating_sub(viewport))
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
    /// v4.0 P4: the fenced-body line-number slot (right-padded, or blank
    /// spaces on wrapped continuations). `None` outside code fences.
    pub code_line: Option<String>,
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
    if let Some(header) = &cell.header {
        return vec![project_card_header(header, width)];
    }
    if let Some(body) = &cell.body {
        return project_card_body(body, width, false);
    }
    let mut rows = match cell.kind {
        // User rows reserve the two cells of the ❯ prefix so wrapped
        // continuations hang past it (v4.0 P2 §4).
        LineKind::User => plain_rows(text::wrap(&cell.text, width.saturating_sub(2).max(1))),
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

// ---------------------------------------------------------------------------
// Tool-card projection (v4.0 P3)
// ---------------------------------------------------------------------------

/// Converts a card segment color to the shared prose color vocabulary.
fn seg_color(color: SegColor) -> ProseColor {
    match color {
        SegColor::Default => ProseColor::Default,
        SegColor::Gray => ProseColor::Meta,
        SegColor::DarkGray => ProseColor::DarkGray,
        SegColor::Green => ProseColor::Green,
        SegColor::Yellow => ProseColor::Yellow,
        SegColor::Orange => ProseColor::Code,
        SegColor::Red => ProseColor::Red,
        SegColor::Border => ProseColor::Border,
    }
}

/// Maps one card segment onto a baked prose span.
fn seg_span(seg: &SegSpan) -> ProseSpan {
    ProseSpan {
        text: seg.text.clone(),
        style: ProseStyle {
            color: seg_color(seg.color),
            bold: seg.bold,
            ..ProseStyle::default()
        },
    }
}

fn header_span(piece: &super::transcript::HeaderPiece) -> ProseSpan {
    ProseSpan {
        text: piece.text.clone(),
        style: ProseStyle {
            color: seg_color(piece.color),
            bold: piece.bold,
            ..ProseStyle::default()
        },
    }
}

fn span_width(span: &ProseSpan) -> usize {
    text::width(&span.text)
}

/// The v4.0 P3 card header, projected into exactly one row.
///
/// Layout: `▎ action target stats` then `·` dots (BORDER) fill to a
/// 2-column gap, then `status time` right-aligned with a 2-column right
/// margin. Narrow-width degradation (§1): middle-truncate the target, drop
/// the stats, then drop the time — the status is never dropped. The row is
/// clipped at `width` as a last resort.
fn project_card_header(header: &CardHeader, width: usize) -> ProjectedRow {
    if width == 0 {
        return ProjectedRow::styled(prose::BlockRole::Plain, Vec::new());
    }
    let bar = &header.bar;
    let action = &header.action;
    let status = &header.status;

    let target_piece: Option<&super::transcript::HeaderPiece> = header.target.as_ref();
    let mut target_text: Option<String> = target_piece.map(|t| t.text.clone());
    let mut stats: Option<&[SegSpan]> = header.stats.as_deref();
    let mut time: Option<&super::transcript::HeaderPiece> = header.time.as_ref();

    const RIGHT_MARGIN: usize = 2;
    const LEADER_GAP: usize = 2;

    let content_width = |target_text: Option<&str>, stats: Option<&[SegSpan]>| {
        text::width(&bar.text)
            + 1
            + text::width(&action.text)
            + target_text.map_or(0, |t| 1 + text::width(t))
            + stats
                .map_or(0, |segs| 1 + segs.iter().map(|s| text::width(&s.text)).sum::<usize>())
    };
    let suffix_width = |time: Option<&super::transcript::HeaderPiece>| {
        text::width(&status.text) + time.map_or(0, |t| 1 + text::width(&t.text))
    };
    let dots_for = |content: usize, suffix: usize| {
        width.saturating_sub(RIGHT_MARGIN + LEADER_GAP + content + suffix)
    };

    let mut leader = dots_for(content_width(target_text.as_deref(), stats), suffix_width(time));

    // Degradation ladder (§1): middle-truncate the target, drop the stats,
    // then drop the time. The status survives every step.
    if leader < 1 {
        if let (Some(_), Some(text)) = (target_piece, target_text.as_deref()) {
            let text_width = text::width(text);
            if text_width >= 3 {
                let reclaim = 1 - leader;
                let truncated = text::truncate_mid(text, text_width.saturating_sub(reclaim));
                if dots_for(content_width(Some(&truncated), stats), suffix_width(time)) >= 1 {
                    target_text = Some(truncated);
                }
            }
        }
        leader = dots_for(content_width(target_text.as_deref(), stats), suffix_width(time));
        if leader < 1 && stats.is_some() {
            stats = None;
            leader = dots_for(content_width(target_text.as_deref(), stats), suffix_width(time));
        }
        if leader < 1 && time.is_some() {
            time = None;
            leader = dots_for(content_width(target_text.as_deref(), stats), suffix_width(None));
        }
        if leader < 1 {
            leader = 0;
        }
    }

    let mut spans: Vec<ProseSpan> = Vec::new();
    spans.push(header_span(bar));
    spans.push(ProseSpan::new(" ", ProseStyle::default()));
    spans.push(header_span(action));
    if let (Some(piece), Some(text)) = (target_piece, target_text.as_ref()) {
        spans.push(ProseSpan::new(" ", ProseStyle::default()));
        let mut target = piece.clone();
        target.text.clone_from(text);
        spans.push(header_span(&target));
    }
    if let Some(s) = stats {
        spans.push(ProseSpan::new(" ", ProseStyle::default()));
        spans.extend(s.iter().map(seg_span));
    }
    if leader > 0 {
        spans.push(ProseSpan::new(" ", ProseStyle::default()));
        spans.push(ProseSpan::new(
            "·".repeat(leader),
            ProseStyle::default().colored(ProseColor::Border),
        ));
        spans.push(ProseSpan::new(" ", ProseStyle::default()));
    }
    spans.push(header_span(status));
    if let Some(t) = time {
        spans.push(ProseSpan::new(" ", ProseStyle::default()));
        spans.push(header_span(t));
    }
    let row = ProjectedRow::styled(prose::BlockRole::Plain, spans);
    if text::width(&row.text) > width {
        // Absolute floor: the whole row collapses to one clipped fragment.
        return ProjectedRow::styled(
            prose::BlockRole::Plain,
            vec![ProseSpan::new(
                text::truncate(&row.text, width),
                ProseStyle::default(),
            )],
        );
    }
    row
}

/// The card body: typed rows under a header, foldable past the threshold.
fn project_card_body(body: &CardBody, width: usize, folded: bool) -> Vec<ProjectedRow> {
    if width == 0 {
        return Vec::new();
    }
    let line_count = body.lines.len();
    let mut rows = Vec::new();
    if folded && line_count > body.threshold {
        if body.fold_all {
            rows.push(project_fold_bar(body, width));
        } else {
            for line in body.lines.iter().take(2) {
                rows.extend(project_body_line(line, width));
            }
            rows.push(project_fold_bar(body, width));
            if let Some(last) = body.lines.last() {
                rows.extend(project_body_line(last, width));
            }
        }
    } else {
        for line in &body.lines {
            rows.extend(project_body_line(line, width));
        }
    }
    rows
}

/// One body row: a 2-column indent (the §2 fixed right shift) plus the
/// line's segments, soft-wrapped with the gutter as hanging indent when it
/// overflows the width. Diff del/ins rows carry their wash tone.
fn project_body_line(segments: &[SegSpan], width: usize) -> Vec<ProjectedRow> {
    let indent = ProseSpan::new("  ", ProseStyle::default());
    let gutter = segments
        .first()
        .map(seg_span)
        .unwrap_or_else(|| ProseSpan::new("", ProseStyle::default()));
    let mut spans: Vec<ProseSpan> = Vec::with_capacity(segments.len() + 1);
    spans.push(indent);
    spans.extend(segments.iter().map(seg_span));
    let wash = segments.iter().find_map(|seg| seg.tone);
    let finish = |row: Vec<ProseSpan>| match wash {
        Some(tone) => ProjectedRow::styled_with_tone(prose::BlockRole::Plain, row, tone),
        None => ProjectedRow::styled(prose::BlockRole::Plain, row),
    };
    if spans_width(&spans) <= width {
        return vec![finish(spans)];
    }
    let indent_width = 2 + span_width(&gutter);
    let mut rows = prose::wrap_spans(&spans, width);
    if rows.len() > 1 && indent_width > 0 && indent_width < width {
        let pad = ProseSpan::new(" ".repeat(indent_width), ProseStyle::default());
        for row in &mut rows[1..] {
            let mut full = vec![pad.clone()];
            full.append(row);
            *row = full;
        }
    }
    rows.into_iter().map(finish).collect()
}

fn spans_width(spans: &[ProseSpan]) -> usize {
    spans.iter().map(span_width).sum()
}

/// The fold bar row: `  ┈┈ ▾ N {label} (按 Space 展开) ┈┈`, rails GRAY, the
/// count ORANGE, the Space hint DARK_GRAY, extended with `┈` to the width.
/// Sits inside the body's 2-column indent like every other body row.
fn project_fold_bar(body: &CardBody, width: usize) -> ProjectedRow {
    let count = body.fold_count;
    let mut spans = Vec::new();
    spans.push(ProseSpan::new("  ", ProseStyle::default()));
    spans.push(ProseSpan::new(
        "┈┈ ",
        ProseStyle::default().colored(ProseColor::Meta),
    ));
    let center = format!("▾ {count} {}", body.fold_label);
    spans.push(ProseSpan::new(
        center.clone(),
        ProseStyle::default().colored(ProseColor::Code),
    ));
    let hint = if body.fold_hint {
        let hint = " (按 Space 展开)".to_owned();
        spans.push(ProseSpan::new(
            hint.clone(),
            ProseStyle::default().colored(ProseColor::DarkGray),
        ));
        hint
    } else {
        String::new()
    };
    let mut right = " ┈┈".to_owned();
    let used = 2 + text::width("┈┈ ") + text::width(&center) + text::width(&hint) + text::width(&right);
    if width > used {
        right.push_str(&"┈".repeat(width - used));
    }
    spans.push(ProseSpan::new(
        right,
        ProseStyle::default().colored(ProseColor::Meta),
    ));
    ProjectedRow::styled(prose::BlockRole::Plain, spans)
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

/// The think block's header row (v4.0 P3 §6 re-skin): `▎ Thought for 4.2s
/// ··········· (按 Space 查看)`. Whole row in the gray family — `▎` and the
/// duration DARK_GRAY, `Thought for` gray, dot leaders (BORDER) fill the
/// width, the Space hint DARK_GRAY. Replay (no timing) renders a bare
/// `Thought`.
fn project_think_header(elapsed: Option<Duration>, width: usize) -> ProjectedRow {
    if width == 0 {
        return ProjectedRow::styled(prose::BlockRole::Plain, Vec::new());
    }
    let timed = elapsed.is_some();
    let label = if timed { "Thought for " } else { "Thought" };
    let duration_text = match elapsed {
        Some(elapsed) => crate::app::run_state::format_think_elapsed(elapsed),
        None => String::new(),
    };
    let hint = " (按 Space 查看)";
    // `▎ ` + label [+ duration] + ` ` + `·` dots + ` ` + hint.
    let fixed = text::width("▎ ")
        + text::width(label)
        + text::width(&duration_text)
        + 2
        + text::width(hint);
    let dots = width.saturating_sub(fixed);
    let mut spans = Vec::new();
    spans.push(ProseSpan::new(
        "▎ ",
        ProseStyle::default().colored(ProseColor::DarkGray),
    ));
    spans.push(ProseSpan::new(
        label,
        ProseStyle::default().colored(ProseColor::Meta),
    ));
    if !duration_text.is_empty() {
        spans.push(ProseSpan::new(
            duration_text,
            ProseStyle::default().colored(ProseColor::DarkGray),
        ));
    }
    spans.push(ProseSpan::new(" ", ProseStyle::default()));
    spans.push(ProseSpan::new(
        "·".repeat(dots),
        ProseStyle::default().colored(ProseColor::Border),
    ));
    spans.push(ProseSpan::new(" ", ProseStyle::default()));
    spans.push(ProseSpan::new(
        hint,
        ProseStyle::default().colored(ProseColor::DarkGray),
    ));
    ProjectedRow::styled(prose::BlockRole::Plain, spans)
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
    if cell.kind == LineKind::Tool {
        if cell.header.is_none() && cell.body.is_none() {
            return wrap_line(cell, width, prev);
        }
        let mut rows = Vec::new();
        if let Some(header) = &cell.header {
            rows.push(project_card_header(header, width));
        }
        if let Some(body) = &cell.body {
            let folded = collapsed(index) && body.lines.len() > body.threshold;
            rows.extend(project_card_body(body, width, folded));
        }
        return rows;
    }
    if cell.kind == LineKind::Reasoning && cell.text == "think" {
        return vec![project_think_header(think_elapsed, width)];
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

pub(crate) fn row_count(wrapped: &[Vec<ProjectedRow>]) -> usize {
    wrapped.iter().map(Vec::len).sum()
}

pub(crate) fn start_from_tail(wrapped: &[Vec<ProjectedRow>], height: usize) -> (usize, usize) {
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

pub(crate) fn clamp_pin(pin: Option<(usize, usize)>, wrapped: &[Vec<ProjectedRow>]) -> (usize, usize) {
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

pub(crate) fn rows_from(wrapped: &[Vec<ProjectedRow>], cell: usize, row: usize) -> usize {
    if cell >= wrapped.len() {
        return 0;
    }
    let first = wrapped[cell].len().saturating_sub(row);
    first + wrapped[cell + 1..].iter().map(Vec::len).sum::<usize>()
}

/// Clamps a logical cursor position onto the wrapped rows.
pub(crate) fn clamp_cursor(
    wrapped: &[Vec<ProjectedRow>],
    pos: (usize, usize),
) -> (usize, usize) {
    clamp_pin(Some(pos), wrapped)
}

/// Absolute wrapped-row index of a logical position (the scrollbar's S
/// space): rows before the cursor, 0 at the transcript head.
pub(crate) fn logical_index(
    wrapped: &[Vec<ProjectedRow>],
    pos: (usize, usize),
) -> usize {
    row_count(wrapped).saturating_sub(rows_from(wrapped, pos.0, pos.1))
}

/// The logical position whose wrapped row sits at absolute `index`.
/// Past-the-end indices clamp onto the last row.
pub(crate) fn position_at_index(wrapped: &[Vec<ProjectedRow>], mut index: usize) -> (usize, usize) {
    if wrapped.is_empty() {
        return (0, 0);
    }
    for (cell, rows) in wrapped.iter().enumerate() {
        if index < rows.len() {
            return (cell, index);
        }
        index -= rows.len();
    }
    let last = wrapped.len() - 1;
    (last, wrapped[last].len().saturating_sub(1))
}

/// Moves a logical cursor by `delta` wrapped rows (browse-mode stepping),
/// clamping at both ends of the transcript.
pub(crate) fn move_cursor(
    wrapped: &[Vec<ProjectedRow>],
    pos: (usize, usize),
    delta: isize,
) -> (usize, usize) {
    if wrapped.is_empty() {
        return (0, 0);
    }
    let (mut cell, mut row) = clamp_cursor(wrapped, pos);
    if delta < 0 {
        move_backward(wrapped, &mut cell, &mut row, delta.unsigned_abs());
    } else if delta > 0 {
        move_forward(wrapped, &mut cell, &mut row, delta as usize);
    }
    (cell, row)
}

fn window_reaches_tail(
    wrapped: &[Vec<ProjectedRow>],
    cell: usize,
    row: usize,
    height: usize,
) -> bool {
    wrapped.is_empty() || rows_from(wrapped, cell, row) <= height
}

pub(crate) fn move_backward(
    wrapped: &[Vec<ProjectedRow>],
    cell: &mut usize,
    row: &mut usize,
    mut n: usize,
) {
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

pub(crate) fn move_forward(
    wrapped: &[Vec<ProjectedRow>],
    cell: &mut usize,
    row: &mut usize,
    mut n: usize,
) {
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
                tone: rows[row].tone.unwrap_or(cells[cell].tone),
                role: rows[row].role.clone(),
                spans: rows[row].spans.clone(),
                code_line: rows[row].code_line.clone(),
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
                tone: rows[row].tone.unwrap_or(store.display_tone(cell)),
                role: rows[row].role.clone(),
                spans: rows[row].spans.clone(),
                code_line: rows[row].code_line.clone(),
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

    /// The v4.0 P3 re-skinned think header row.
    fn is_think_header(text: &str) -> bool {
        text.starts_with("▎ Thought") && text.contains("按 Space 查看")
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
    fn user_rows_wrap_two_short_reserving_the_prompt_prefix() {
        let cells = vec![line(LineKind::User, "abcdefgh")];
        let scroll = ScrollState::follow();
        // Width 6 reserves two columns for the painted `❯ ` prefix, so the
        // text wraps at 4 with continuations hanging past the glyph.
        let slice = visible_slice(&cells, 6, 2, &scroll);
        assert_eq!(texts(&slice), ["abcd", "efgh"]);
    }

    #[test]
    fn block_boundaries_get_a_leading_gap_row() {
        let tool = |text: &str, tone: Tone| crate::app::transcript::TranscriptLine {
            kind: LineKind::Tool,
            text: text.to_owned(),
            tone,
            header: None,
            body: None,
        };
        let cells = vec![
            line(LineKind::Answer, "abcdefgh"),
            line(LineKind::Answer, "ijklmnop"),
            tool("Grep 1 search", Tone::Title),
            tool("  \"hit\"", Tone::Detail),
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
                "  \"hit",
                "  \"",
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
        let folded = texts(&slice);
        assert_eq!(folded.len(), 3, "the folded run renders one header row: {folded:?}");
        assert_eq!(folded[0], "a");
        assert_eq!(folded[2], "tail");
        assert!(
            is_think_header(folded[1]),
            "the folded run renders the think header: {folded:?}"
        );

        let mut store = TranscriptStore::default();
        store.push_closed([crate::app::transcript::line(LineKind::Reasoning, "think")]);
        store.refresh_wraps(80, &|_| true);
        let slice = store.visible_slice(80, 10, &ScrollState::follow(), &|_| true);
        assert!(is_think_header(texts(&slice)[0]));
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
            let head_text = row_texts(&rows[head])[0];
            assert!(
                head_text.contains("Thought for 8s") && head_text.contains("按 Space 查看"),
                "timed header: {head_text:?}"
            );
            assert_eq!(row_texts(&rows[2]), ["after"]);
        }

        // Sealed: the frozen span survives the cache rebuild.
        store.seal_think();
        store.freeze_think(std::time::Duration::from_secs(8));
        store.refresh_wraps(80, &|_| false);
        {
            let rows = store.wrap_rows();
            let head_text = row_texts(&rows[head])[0];
            assert!(
                head_text.contains("Thought for 8s") && head_text.contains("按 Space 查看"),
                "timed header after seal: {head_text:?}"
            );
            assert_eq!(row_texts(&rows[1]), ["│ body"]);
        }

        // Untimed blocks (replay) keep the bare header.
        let mut replay = TranscriptStore::default();
        replay.push_closed([crate::app::transcript::line(LineKind::Reasoning, "think")]);
        replay.refresh_wraps(80, &|_| true);
        let replay_rows = replay.wrap_rows();
        let replay_text = row_texts(&replay_rows[0])[0];
        assert!(
            replay_text.starts_with("▎ Thought ") && replay_text.contains("按 Space 查看"),
            "untimed header: {replay_text:?}"
        );
    }

    #[test]
    fn fold_toggles_rebuild_cached_rows() {
        let mut store = TranscriptStore::default();
        store.push_closed([crate::app::transcript::line(LineKind::Reasoning, "think")]);
        store.refresh_wraps(80, &|_| true);
        {
            let rows = store.wrap_rows();
            assert!(is_think_header(row_texts(&rows[0])[0]));
        }
        store.bump_wrap_revision();
        store.refresh_wraps(80, &|_| false);
        {
            let rows = store.wrap_rows();
            assert!(is_think_header(row_texts(&rows[0])[0]));
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
