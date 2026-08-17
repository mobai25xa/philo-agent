//! Ordered transcript cells with an in-place open cursor, and the wrap slice.
//!
//! `TranscriptLine` is the Step-1 cell. Wrapped rows are derived at the
//! current width and are never the source of truth. In-progress Answer/Think
//! is a real cell at its insertion point (`open` is always the last index).
//! Wrap rows are cached per width: the closed prefix is stable; a width
//! change rebuilds everything; the open cell is always rewrapped.

use std::cell::{Ref, RefCell};

use super::text;
use super::transcript::{LineKind, TranscriptLine};

#[derive(Clone, Debug)]
struct WrapCache {
    width: usize,
    closed_upto: usize,
    rows: Vec<Vec<String>>,
}

impl Default for WrapCache {
    fn default() -> Self {
        Self {
            width: usize::MAX,
            closed_upto: 0,
            rows: Vec::new(),
        }
    }
}

impl WrapCache {
    fn invalidate(&mut self) {
        *self = Self::default();
    }
}

/// Canonical history for one TUI session: one ordered cell list plus an
/// optional in-place open cursor on the last cell.
#[derive(Clone, Debug, Default)]
pub(crate) struct TranscriptStore {
    cells: Vec<TranscriptLine>,
    open: Option<usize>,
    cache: RefCell<WrapCache>,
}

impl TranscriptStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.cells.clear();
        self.open = None;
        self.cache.borrow_mut().invalidate();
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

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn display_len(&self) -> usize {
        self.cells.len()
    }

    pub fn display_kind(&self, index: usize) -> LineKind {
        self.cells[index].kind
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

    /// Rebuild wrap rows when the width changes. The closed prefix is
    /// stable; always rewrap from `open_index.unwrap_or(len)`.
    pub(crate) fn refresh_wraps(&self, width: usize) {
        self.assert_open_last();
        let mut cache = self.cache.borrow_mut();
        if width == 0 {
            cache.invalidate();
            cache.width = 0;
            return;
        }
        if cache.width != width {
            cache.invalidate();
            cache.width = width;
        }
        let rewrap_from = self.open.unwrap_or(self.cells.len());
        if cache.closed_upto > rewrap_from {
            cache.rows.truncate(rewrap_from);
            cache.closed_upto = rewrap_from;
        }
        if cache.closed_upto > self.cells.len() {
            cache.rows.truncate(self.cells.len());
            cache.closed_upto = self.cells.len();
        }
        let start = cache.closed_upto.min(rewrap_from);
        cache.rows.truncate(start);
        cache.closed_upto = start;
        for cell in &self.cells[start..rewrap_from] {
            cache.rows.push(wrap_line(cell, width, None));
            cache.closed_upto += 1;
        }
        for cell in &self.cells[rewrap_from..] {
            cache.rows.push(wrap_line(cell, width, None));
        }
    }

    pub(crate) fn wrap_rows(&self) -> Ref<'_, [Vec<String>]> {
        Ref::map(self.cache.borrow(), |cache| cache.rows.as_slice())
    }

    pub(crate) fn visible_slice(
        &self,
        width: usize,
        height: usize,
        scroll: &ScrollState,
    ) -> VisibleSlice {
        self.refresh_wraps(width);
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

    pub(crate) fn scroll_wrapped(&mut self, wrapped: &[Vec<String>], height: usize, delta: isize) {
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

    pub(crate) fn unfollow_keep_wrapped(&mut self, wrapped: &[Vec<String>], height: usize) {
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

    pub(crate) fn jump_top(&mut self, wrapped: &[Vec<String>], height: usize) {
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

/// Pure. No Ratatui. No MarkdownRenderer.
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
    wrapped: &[Vec<String>],
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
    wrapped: &[Vec<String>],
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
    _prev: Option<LineKind>,
) -> Vec<String> {
    match cell.kind {
        LineKind::User => text::wrap_user(&cell.text, width),
        LineKind::Answer => text::wrap_answer(&cell.text, width, true),
        LineKind::Tool => text::wrap_hanging(&cell.text, width),
        LineKind::Reasoning => text::wrap_reasoning(&cell.text, width),
        _ => text::wrap(&cell.text, width),
    }
}

#[cfg(test)]
fn wrap_all(cells: &[TranscriptLine], width: usize) -> Vec<Vec<String>> {
    cells
        .iter()
        .map(|cell| wrap_line(cell, width, None))
        .collect()
}

fn row_count(wrapped: &[Vec<String>]) -> usize {
    wrapped.iter().map(Vec::len).sum()
}

fn start_from_tail(wrapped: &[Vec<String>], height: usize) -> (usize, usize) {
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

fn clamp_pin(pin: Option<(usize, usize)>, wrapped: &[Vec<String>]) -> (usize, usize) {
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

fn rows_from(wrapped: &[Vec<String>], cell: usize, row: usize) -> usize {
    if cell >= wrapped.len() {
        return 0;
    }
    let first = wrapped[cell].len().saturating_sub(row);
    first + wrapped[cell + 1..].iter().map(Vec::len).sum::<usize>()
}

fn window_reaches_tail(wrapped: &[Vec<String>], cell: usize, row: usize, height: usize) -> bool {
    wrapped.is_empty() || rows_from(wrapped, cell, row) <= height
}

fn move_backward(wrapped: &[Vec<String>], cell: &mut usize, row: &mut usize, mut n: usize) {
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

fn move_forward(wrapped: &[Vec<String>], cell: &mut usize, row: &mut usize, mut n: usize) {
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
    wrapped: &[Vec<String>],
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
                text: rows[row].clone(),
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
    wrapped: &[Vec<String>],
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
                text: rows[row].clone(),
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
    use crate::app::transcript::line;

    fn meta(text: &str) -> TranscriptLine {
        line(LineKind::Meta, text)
    }

    fn texts(slice: &VisibleSlice) -> Vec<&str> {
        slice.rows.iter().map(|row| row.text.as_str()).collect()
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
    fn user_lines_wrap_with_a_hanging_gutter() {
        let cells = vec![line(LineKind::User, "› abcdefgh")];
        let scroll = ScrollState::follow();
        let slice = visible_slice(&cells, 6, 2, &scroll);
        assert_eq!(texts(&slice), ["› abcd", "  efgh"]);
    }

    #[test]
    fn each_answer_cell_gets_its_own_lead_bullet() {
        let cells = vec![
            line(LineKind::Answer, "abcdefgh"),
            line(LineKind::Answer, "ijklmnop"),
            line(LineKind::Tool, "▸ read"),
            line(LineKind::Answer, "next"),
        ];
        let scroll = ScrollState::follow();
        let slice = visible_slice(&cells, 6, 8, &scroll);
        assert_eq!(
            texts(&slice),
            ["• abcd", "  efgh", "• ijkl", "  mnop", "▸ read", "• next"]
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
        store.refresh_wraps(4);
        assert_eq!(store.cached_width(), 4);
        assert_eq!(store.cached_closed_upto(), 3);
        let narrow = store.wrap_rows().to_vec();
        assert_eq!(narrow[0], ["中文", "0"]);

        store.refresh_wraps(4);
        assert_eq!(
            store.cached_closed_upto(),
            3,
            "same width reuses closed wraps"
        );
        assert_eq!(&*store.wrap_rows(), narrow.as_slice());

        store.refresh_wraps(80);
        assert_eq!(store.cached_width(), 80);
        assert_eq!(store.wrap_rows()[0], ["中文0"]);

        store.refresh_wraps(4);
        assert_eq!(&*store.wrap_rows(), narrow.as_slice());

        store.push_closed([meta("中文3")]);
        store.refresh_wraps(4);
        assert_eq!(store.cached_closed_upto(), 4);
        assert_eq!(store.wrap_rows()[3], ["中文", "3"]);

        store.begin(LineKind::Meta, "中文");
        store.refresh_wraps(4);
        assert_eq!(store.cached_closed_upto(), 4);
        assert_eq!(store.wrap_rows()[4], ["中文"]);
        store.write_open("live");
        store.refresh_wraps(4);
        assert_eq!(
            store.cached_closed_upto(),
            4,
            "open never advances closed cache"
        );
        assert_eq!(store.wrap_rows()[4], ["中文", "live"]);
    }
}
