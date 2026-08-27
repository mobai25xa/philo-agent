//! Side scrollbar (new-tui.md §9 / P2 task §1): pure geometry plus the
//! one-column rail painter.
//!
//! The math is authoritative — do not "improve" it:
//!
//! ```text
//! ThumbHeight = max(1, floor(V * V / T))
//! ThumbTop    = floor(S / (T - V) * (V - ThumbHeight))
//! ```
//!
//! Content that does not overflow (`T <= V`) draws only the quiet track.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::theme;

const TRACK: &str = "│";
const THUMB: &str = "█";

/// Thumb geometry for a viewport of `viewport` rows over `total` wrapped
/// rows at scroll offset `offset`. Returns `None` when the content does not
/// overflow (the rail stays a bare track).
pub(crate) fn thumb(total: usize, viewport: usize, offset: usize) -> Option<(usize, usize)> {
    if total == 0 || viewport == 0 || total <= viewport {
        return None;
    }
    let v = viewport;
    let height = usize::max(1, v * v / total).min(v);
    // No-overflow is handled above; `span` and `travel` are therefore >= 1
    // (span) and >= 1 whenever the clamp below would matter — offsets are
    // additionally clamped so pathological inputs stay in range.
    let span = total - v;
    let travel = v - height;
    let top = offset.min(span) * travel / span;
    Some((top, height))
}

/// Thumb color: highlighted while a scroll happened recently.
fn thumb_color(active: bool) -> Color {
    if active {
        theme::meta().fg.unwrap_or(Color::Reset)
    } else {
        theme::thumb_idle_color()
    }
}

/// Paints the full rail column for the transcript band (`rail`), whose
/// height is the viewport V and whose top aligns with the band top.
/// `offset`/`total` describe the scrolled window in wrapped rows.
pub(crate) fn paint(
    frame: &mut ratatui::Frame<'_>,
    rail: Rect,
    total: usize,
    offset: usize,
    active: bool,
) {
    if rail.is_empty() || rail.width < 1 {
        return;
    }
    let viewport = usize::from(rail.height);
    let active_style = Style::default().fg(thumb_color(active));
    let track_style = Style::default().fg(theme::track_color());
    match thumb(total, viewport, offset) {
        None => {
            frame.render_widget(
                Paragraph::new(rail_column(TRACK, track_style, rail.height)),
                rail,
            );
        }
        Some((top, height)) => {
            let lines = (0..rail.height)
                .map(|row| {
                    let row = usize::from(row);
                    let inside = row >= top && row < top + height;
                    Line::from(Span::styled(
                        if inside { THUMB } else { TRACK },
                        if inside { active_style } else { track_style },
                    ))
                })
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines), rail);
        }
    }
}

fn rail_column(glyph: &str, style: Style, rows: u16) -> Vec<Line<'static>> {
    (0..rows)
        .map(|_| Line::from(Span::styled(glyph.to_owned(), style)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_overflow_draws_no_thumb() {
        assert_eq!(thumb(10, 10, 0), None);
        assert_eq!(thumb(5, 10, 0), None);
        assert_eq!(thumb(0, 0, 0), None);
    }

    #[test]
    fn thumb_height_is_at_least_one_row() {
        let (_, height) = thumb(100_000, 5, 0).expect("overflow");
        assert_eq!(height, 1);
    }

    #[test]
    fn full_history_short_of_one_page_caps_the_thumb() {
        let (top, height) = thumb(11, 10, 0).expect("overflow");
        assert_eq!(height, 9, "floor(10*10/11)");
        assert_eq!(top, 0);
    }

    #[test]
    fn half_scrolled_viewport_centers_the_thumb() {
        let (top, height) = thumb(100, 10, 45).expect("overflow");
        assert_eq!(height, 1);
        // S/(T-V)*(V-H) = 45/90*9 = 4.5 → 4
        assert_eq!(top, 4);
    }

    #[test]
    fn bottom_scroll_pins_the_thumb_to_the_rail_floor() {
        let (top, _) = thumb(100, 10, 90).expect("overflow");
        assert_eq!(top, 9, "S=T-V rides the last track row");
    }

    #[test]
    fn offsets_beyond_the_scroll_span_clamp_into_range() {
        // T=20 V=10 → H=5, travel=5; clamping S to T-V=10 → top=10/10*5=5.
        let (top, _) = thumb(20, 10, 999).expect("overflow");
        assert_eq!(top, 5);
    }

    #[test]
    fn square_proportion_wears_a_half_rail_thumb() {
        // T=40, V=20 → H=floor(400/40)=10; top at S=10 → 10/20*10=5.
        let (top, height) = thumb(40, 20, 10).expect("overflow");
        assert_eq!(height, 10);
        assert_eq!(top, 5);
    }
}
