//! Transcript selection in wrapped-cell coordinates.
//!
//! The range is `(cell, row_in_cell, column)`, the same space as the scroll
//! pin. `cell` is an index into the display list (sealed + unsealed).
//! Native terminal selection is not used: mouse capture owns the screen.

use super::cells::VisibleSlice;
use super::text;
use super::transcript::TranscriptLine;

/// A rectangular region of the isolated screen, in terminal cells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BandLayout {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl BandLayout {
    pub(crate) fn from_parts(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub(crate) fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub(crate) fn contains(self, x: u16, y: u16) -> bool {
        !self.is_empty()
            && x >= self.x
            && y >= self.y
            && x < self.x.saturating_add(self.width)
            && y < self.y.saturating_add(self.height)
    }

    /// Column and row relative to the band origin.
    pub(crate) fn relative(self, x: u16, y: u16) -> Option<(usize, usize)> {
        if !self.contains(x, y) {
            return None;
        }
        Some((
            usize::from(x.saturating_sub(self.x)),
            usize::from(y.saturating_sub(self.y)),
        ))
    }

    pub(crate) fn above(self, y: u16) -> bool {
        !self.is_empty() && y < self.y
    }

    pub(crate) fn below(self, y: u16) -> bool {
        !self.is_empty() && y >= self.y.saturating_add(self.height)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SelectPos {
    pub cell: usize,
    pub row: usize,
    pub col: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Selection {
    pub anchor: SelectPos,
    pub head: SelectPos,
    pub dragging: bool,
}

impl Selection {
    pub(crate) fn start(pos: SelectPos) -> Self {
        Self {
            anchor: pos,
            head: pos,
            dragging: true,
        }
    }

    pub(crate) fn normalized(self) -> (SelectPos, SelectPos) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    pub(crate) fn is_collapsed(self) -> bool {
        self.anchor == self.head
    }

    /// Inclusive-start, exclusive-end columns on one wrapped row, if it
    /// intersects this selection. Empty middle rows still return `(0, 0)`
    /// so a visual-line copy keeps the blank line.
    pub(crate) fn columns_on_row(
        self,
        cell: usize,
        row: usize,
        row_width: usize,
    ) -> Option<(usize, usize)> {
        let (start, end) = self.normalized();
        let here = (cell, row);
        if here < (start.cell, start.row) || here > (end.cell, end.row) {
            return None;
        }
        if here > (start.cell, start.row) && here < (end.cell, end.row) {
            return Some((0, row_width));
        }
        let from = if here == (start.cell, start.row) {
            start.col.min(row_width)
        } else {
            0
        };
        let to = if here == (end.cell, end.row) {
            end.col.min(row_width)
        } else {
            row_width
        };
        Some((from, to.max(from)))
    }
}

/// Keep a pointer inside the current wrap of the display list.
pub(crate) fn clamp_pos(pos: SelectPos, cells: &[TranscriptLine], width: usize) -> SelectPos {
    if cells.is_empty() {
        return SelectPos {
            cell: 0,
            row: 0,
            col: 0,
        };
    }
    let index = pos.cell.min(cells.len() - 1);
    let wrapped = if width == 0 {
        vec![String::new()]
    } else {
        text::wrap(&cells[index].text, width)
    };
    let row = if wrapped.is_empty() {
        0
    } else {
        pos.row.min(wrapped.len() - 1)
    };
    let col = wrapped
        .get(row)
        .map(|text| pos.col.min(text::width(text)))
        .unwrap_or(0);
    SelectPos {
        cell: index,
        row,
        col,
    }
}

pub(crate) fn hit_history(slice: &VisibleSlice, col: usize, row: usize) -> Option<SelectPos> {
    if slice.rows.is_empty() {
        return None;
    }
    let index = row.min(slice.rows.len() - 1);
    let visible = &slice.rows[index];
    let width = text::width(&visible.text);
    let col = if row >= slice.rows.len() {
        width
    } else {
        col.min(width)
    };
    Some(SelectPos {
        cell: visible.cell_index,
        row: visible.row_in_cell,
        col,
    })
}

/// Visual-line copy: selected wrapped rows joined by newlines.
pub(crate) fn extract_text(cells: &[TranscriptLine], width: usize, selection: Selection) -> String {
    let (start, end) = selection.normalized();
    if start == end || width == 0 {
        return String::new();
    }
    let mut parts = Vec::new();
    for (index, cell) in cells.iter().enumerate() {
        let wrapped = text::wrap(&cell.text, width);
        for (row, text) in wrapped.iter().enumerate() {
            if let Some((from, to)) = selection.columns_on_row(index, row, text::width(text)) {
                parts.push(text::slice_columns(text, from, to));
            }
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::cells::{ScrollState, visible_slice};
    use crate::app::transcript::{LineKind, line};

    fn meta(text: &str) -> TranscriptLine {
        line(LineKind::Meta, text)
    }

    #[test]
    fn extract_walks_wrapped_rows_as_visual_lines() {
        let cells = vec![meta("中文ab")];
        let selection = Selection {
            anchor: SelectPos {
                cell: 0,
                row: 0,
                col: 0,
            },
            head: SelectPos {
                cell: 0,
                row: 1,
                col: 2,
            },
            dragging: false,
        };
        assert_eq!(extract_text(&cells, 4, selection), "中文\nab");
    }

    #[test]
    fn collapsed_selection_copies_nothing() {
        let cells = vec![meta("hello")];
        let pos = SelectPos {
            cell: 0,
            row: 0,
            col: 1,
        };
        let selection = Selection {
            anchor: pos,
            head: pos,
            dragging: false,
        };
        assert!(extract_text(&cells, 80, selection).is_empty());
    }

    #[test]
    fn hit_test_maps_a_visible_row() {
        let cells: Vec<_> = (0..10).map(|i| meta(&format!("row-{i}"))).collect();
        let slice = visible_slice(&cells, 80, 3, &ScrollState::follow());
        let pos = hit_history(&slice, 0, 0).expect("hit");
        assert_eq!(pos.cell, 7);
        assert_eq!(pos.row, 0);
        assert_eq!(pos.col, 0);
    }

    #[test]
    fn live_sorts_after_sealed_cells() {
        let sealed = meta("ab");
        let unsealed = meta("live");
        assert!(0 < 1, "unsealed display index follows sealed cells");
        let selection = Selection {
            anchor: SelectPos {
                cell: 0,
                row: 0,
                col: 0,
            },
            head: SelectPos {
                cell: 1,
                row: 0,
                col: 4,
            },
            dragging: false,
        };
        let copied = extract_text(&[sealed, unsealed], 80, selection);
        assert_eq!(copied, "ab\nlive");
    }

    #[test]
    fn empty_middle_rows_keep_a_visual_newline() {
        let cells = vec![meta("a"), meta(""), meta("b")];
        let selection = Selection {
            anchor: SelectPos {
                cell: 0,
                row: 0,
                col: 0,
            },
            head: SelectPos {
                cell: 2,
                row: 0,
                col: 1,
            },
            dragging: false,
        };
        assert_eq!(extract_text(&cells, 80, selection), "a\n\nb");
    }

    #[test]
    fn live_without_rows_clamps_onto_the_last_sealed_cell() {
        let cells = vec![meta("xy")];
        let pos = clamp_pos(
            SelectPos {
                cell: 9,
                row: 3,
                col: 9,
            },
            &cells,
            80,
        );
        assert_eq!(
            pos,
            SelectPos {
                cell: 0,
                row: 0,
                col: 2,
            }
        );
    }
}
