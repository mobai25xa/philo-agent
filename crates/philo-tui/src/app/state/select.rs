//! Transcript pointer selection owned by the App.

use super::App;
use crate::app::effect::Effect;
use crate::app::select::{BandLayout, SelectPos, Selection, clamp_pos, extract_text, hit_history};

impl App {
    pub(crate) fn is_selecting(&self) -> bool {
        self.selection.is_some_and(|selection| selection.dragging)
    }

    pub(crate) fn clamped_selection(&self) -> Option<Selection> {
        let mut selection = self.selection?;
        let display = self.cells.display_cells();
        let width = self.layout_width.get();
        selection.anchor = clamp_pos(selection.anchor, &display, width);
        selection.head = clamp_pos(selection.head, &display, width);
        (!selection.is_collapsed()).then_some(selection)
    }

    pub(crate) fn note_transcript_layout(&self, history: BandLayout) {
        self.layout_width.set(usize::from(history.width));
        self.layout_history_height.set(usize::from(history.height));
        self.history_band.set(history);
        self.band_height.set(history.height);
    }

    /// Render pass records the true transcript-band height; the painted
    /// sub-area (which hangs from its tail while streaming) may be shorter.
    pub(crate) fn note_band_height(&self, height: u16) {
        self.band_height.set(height);
    }

    pub(crate) fn note_history_layout(&self, width: usize, history_height: usize) {
        self.note_transcript_layout(BandLayout::from_parts(
            0,
            0,
            sat_u16(width),
            sat_u16(history_height),
        ));
    }

    pub(super) fn clear_selection(&mut self) {
        self.selection = None;
    }

    pub(super) fn select_start(&mut self, x: u16, y: u16) -> Vec<Effect> {
        if self.has_overlay() {
            return vec![];
        }
        match self.pointer_pos(x, y, false) {
            Some(pos) => {
                self.unfollow_keep_view();
                self.selection = Some(Selection::start(pos));
            }
            None => self.clear_selection(),
        }
        vec![]
    }

    pub(super) fn select_drag(&mut self, x: u16, y: u16) -> Vec<Effect> {
        if self.has_overlay() || !self.is_selecting() {
            return vec![];
        }
        if let Some(pos) = self.pointer_pos(x, y, true)
            && let Some(selection) = self.selection.as_mut()
        {
            selection.head = pos;
        }
        vec![]
    }

    pub(super) fn select_end(&mut self, x: u16, y: u16) -> Vec<Effect> {
        if self.has_overlay() || self.selection.is_none() {
            return vec![];
        }
        if self.is_selecting() {
            let pos = self.pointer_pos(x, y, true);
            if let Some(selection) = self.selection.as_mut() {
                if let Some(pos) = pos {
                    selection.head = pos;
                }
                selection.dragging = false;
            }
        }
        if self.selection.is_some_and(Selection::is_collapsed) {
            let anchor = self.selection.unwrap().anchor;
            self.toggle_reasoning_block(anchor.cell, anchor.row);
            self.clear_selection();
        }
        vec![]
    }

    pub(super) fn copy_selection(&self) -> Option<Effect> {
        let selection = self.clamped_selection()?;
        let text = extract_text(
            &self.cells.display_cells(),
            self.layout_width.get(),
            selection,
        );
        (!text.is_empty()).then_some(Effect::WriteClipboard(text))
    }

    fn unfollow_keep_view(&mut self) {
        let height = self.layout_history_height.get();
        if height == 0 {
            return;
        }
        let width = self.layout_width.get();
        self.cells
            .refresh_wraps(width, &|index| self.reasoning_collapsed(index));
        self.scroll
            .unfollow_keep_wrapped(&self.cells.wrap_rows(), height);
    }

    fn pointer_pos(&mut self, x: u16, y: u16, dragging: bool) -> Option<SelectPos> {
        let history = self.history_band.get();
        let width = self.layout_width.get();
        let height = self.layout_history_height.get();

        if dragging {
            if !history.is_empty() && history.above(y) {
                self.scroll_transcript(width, height, -1);
            } else if !history.is_empty() && history.below(y) {
                self.scroll_transcript(width, height, 1);
            }
        }

        if let Some((col, row)) = history.relative(x, y) {
            return hit_history(&self.history_slice(width, height), col, row);
        }
        if dragging {
            return self.edge_pos(x, y);
        }
        None
    }

    fn edge_pos(&self, x: u16, y: u16) -> Option<SelectPos> {
        let history = self.history_band.get();
        if history.is_empty() {
            return None;
        }
        let width = self.layout_width.get();
        let height = self.layout_history_height.get();
        let col = usize::from(x.saturating_sub(history.x));
        if y < history.y {
            return hit_history(&self.history_slice(width, height), col, 0);
        }
        let last = height.saturating_sub(1);
        hit_history(&self.history_slice(width, height), col, last)
    }
}

fn sat_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}
