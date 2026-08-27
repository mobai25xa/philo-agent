//! Pure Ratatui rendering from app state.

use ratatui::layout::Rect;

pub(crate) mod composer;
pub(crate) mod frame;
pub(crate) mod highlight;
pub(crate) mod history;
pub(crate) mod line;
pub(crate) mod markdown;
pub(crate) mod theme;

pub(crate) use theme::CONTENT_INSET;

/// Horizontal inset for the shared content column (transcript rows, corner
/// dashboard text, floats). Bottom chrome stays full-width; composer text
/// uses the same column. The inset value itself is the geometry token
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

/// Horizontal inset for the input band (composer box + user message
/// strips): one column wider than the content column on each side, so the
/// band overhangs the corner rows deliberately (v2.3).
pub(crate) fn inset_band(area: Rect) -> Rect {
    if area.width == 0 {
        return area;
    }
    let pad = theme::BAND_INSET.min(area.width.saturating_sub(1) / 2);
    Rect::new(
        area.x.saturating_add(pad),
        area.y,
        area.width.saturating_sub(pad.saturating_mul(2)),
        area.height,
    )
}

/// Streaming anchor rows (v2.2 §3.2, plan T4.7/T4.8): the 40% and 80%
/// full-screen heights expressed as row offsets into the transcript column
/// (which starts at the screen top). The lifted stream grows from `base`
/// and pins at `cap`; screens too short for a strict ascent return `None`
/// and keep the plain bottom-follow behavior.
pub(crate) fn stream_anchor_rows(screen_height: u16, column_height: u16) -> Option<(u16, u16)> {
    if screen_height == 0 || column_height < 2 {
        return None;
    }
    let hi = column_height - 1;
    let a40 = (screen_height * 2 / 5).clamp(1, hi);
    let a80 = (screen_height * 4 / 5).clamp(1, hi);
    (a40 < a80).then_some((a40, a80))
}

#[cfg(test)]
mod anchor_tests {
    use super::stream_anchor_rows;

    #[test]
    fn anchors_follow_the_screen_shares_and_clamp_into_the_column() {
        assert_eq!(
            stream_anchor_rows(40, 33),
            Some((16, 32)),
            "40%/80% of a tall screen land inside the band"
        );
        assert_eq!(
            stream_anchor_rows(24, 17),
            Some((9, 16)),
            "the 80% share clamps to the column floor"
        );
        assert_eq!(
            stream_anchor_rows(12, 5),
            None,
            "no strict ascent on short screens disables the feature"
        );
        assert_eq!(stream_anchor_rows(0, 10), None);
        assert_eq!(stream_anchor_rows(40, 1), None);
        assert_eq!(stream_anchor_rows(40, 0), None);
    }
}
