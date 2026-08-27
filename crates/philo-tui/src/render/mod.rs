//! Pure Ratatui rendering from app state.

use ratatui::layout::Rect;

pub(crate) mod composer;
pub(crate) mod frame;
pub(crate) mod highlight;
pub(crate) mod history;
pub(crate) mod line;
pub(crate) mod markdown;
pub(crate) mod scrollbar;
pub(crate) mod theme;

pub(crate) use theme::CONTENT_INSET;

/// Horizontal inset for the shared content column (transcript rows, float
/// dialogs). The inset value itself is the geometry token
/// [`theme::CONTENT_INSET`].
pub(crate) fn inset_h(area: Rect) -> Rect {
    if area.width == 0 {
        return area;
    }
    let pad = CONTENT_INSET.min(area.width.saturating_sub(1) / 2);
    Rect::new(
        area.x.saturating_add(pad),
        area.y,
        area.width.saturating_sub(pad.saturating_mul(2)),
        area.height,
    )
}
