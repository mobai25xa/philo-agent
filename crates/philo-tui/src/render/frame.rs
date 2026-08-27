//! Pure projection of app state into the isolated terminal screen.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::app::overlay::{OverlayFrame, OverlayLine};
use crate::app::select::BandLayout;
use crate::app::state::App;
use crate::app::text;

use super::composer;
use super::history;
use super::inset_h;
use super::scrollbar;
use super::theme;

#[cfg(test)]
pub(crate) const VIEWPORT_HEIGHT: u16 = 12;
/// M14's reviewed responsive matrix starts here. Smaller terminals degrade
/// without panicking, but are not part of the supported layout guarantee.
#[cfg(test)]
pub(crate) const MIN_SUPPORTED_WIDTH: u16 = 40;
#[cfg(test)]
pub(crate) const MIN_SUPPORTED_HEIGHT: u16 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PanelAreas {
    live: Rect,
    popover: Rect,
    /// The whole footer band footprint (separator/badge/input/telemetry).
    footer: Rect,
    /// The rounded input box inside the band.
    composer: Rect,
    /// Badge row inside the footer (`state │ model·effort`).
    badge: Option<u16>,
    /// Telemetry row inside the footer (`path │ usage`).
    telemetry: Option<u16>,
}

/// Columns reserved for the scrollbar rail on the right edge (P2 draws
/// the track/thumb; P1 already keeps every content surface off it).
const SCROLLBAR_COLS: u16 = 1;

pub(crate) fn draw(frame: &mut ratatui::Frame<'_>, app: &App, _shift_enter: bool) {
    // v4.0 full-bleed canvas: paint the brand base before anything else so
    // overlays and short bands never expose the native terminal background.
    frame.render_widget(
        Block::default().style(theme::base_fill()),
        frame.area(),
    );
    let canvas = canvas_area(frame.area());
    let areas = panel_areas(canvas, draft_wrapped_rows(app, usize::from(canvas.width)));
    draw_band(frame, app, union(areas.live, areas.popover));
    draw_footer(
        frame,
        app,
        FooterSlots {
            band: areas.footer,
            badge: areas.badge,
            input: areas.composer,
            telemetry: areas.telemetry,
        },
    );
}

/// Wrapped visual-row count of the current draft at `width` — the box's
/// growth ruler (P2 §3.2). Stored rows only; no cursor-follow windowing.
fn draft_wrapped_rows(app: &App, width: usize) -> usize {
    if width < 3 {
        return usize::from(app.input.lines().len().max(1) > 1);
    }
    app.input
        .lines()
        .iter()
        .map(|line| crate::app::text::wrap(line, width).len())
        .sum::<usize>()
        .max(usize::from(!app.input.is_empty()))
}

/// The drawable canvas: the full frame minus the scrollbar's right column.
pub(crate) fn canvas_area(area: Rect) -> Rect {
    let width = area.width.saturating_sub(SCROLLBAR_COLS);
    Rect::new(area.x, area.y, width, area.height)
}

fn union(top: Rect, bottom: Rect) -> Rect {
    if top.height == 0 {
        return bottom;
    }
    if bottom.height == 0 {
        return top;
    }
    Rect::new(
        top.x,
        top.y,
        top.width,
        top.height.saturating_add(bottom.height),
    )
}

/// Fullscreen slot contract (v4.0 P2, new-tui.md §8):
///
/// ```text
/// fullscreen
///   transcript band  = leftover (canvas above the footer)
///   footer separator = 1     (TRACK rule on FOOTER_BG)
///   badge row        = 1     (state │ model·effort)
///   input box        = 3..=8 (rounded box; grows with the draft)
///   telemetry row    = 1     (workspace path │ usage)
/// ```
///
/// Two slots only: transcript and footer band. The composer grows with its
/// draft (`composer::outer_height`, 3↔8 outer) and the transcript gives
/// way; on short screens the band sheds telemetry → badge → separator and
/// finally collapses the box to a bare one-row prompt. Red line: slots
/// never overlap at any supported size.
struct FooterGeometry {
    /// Whole band footprint carved off the canvas bottom.
    band: Rect,
    /// Badge row (`None` when shed).
    badge: Option<u16>,
    /// Rounded input box (or bare line below `MIN`).
    input: Rect,
    /// Telemetry row (`None` when shed).
    telemetry: Option<u16>,
}

/// The footer row set handed to the painter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FooterSlots {
    pub(crate) band: Rect,
    pub(crate) badge: Option<u16>,
    pub(crate) input: Rect,
    pub(crate) telemetry: Option<u16>,
}

/// Live rows reserved below nothing — the minimum transcript height any
/// bordered footer tier must leave above the band.
const MIN_LIVE_ROWS: usize = 3;

fn panel_areas(area: Rect, draft_rows: usize) -> PanelAreas {
    let geometry = footer_geometry(area, draft_rows);
    let live_height = area.height.saturating_sub(geometry.band.height);
    PanelAreas {
        popover: Rect::new(area.x, area.y, area.width, 0),
        live: Rect::new(area.x, area.y, area.width, live_height),
        footer: geometry.band,
        composer: geometry.input,
        badge: geometry.badge,
        telemetry: geometry.telemetry,
    }
}

/// Deterministic footer geometry (P2 §3.2 ladder) for a canvas of `area`
/// rows and a draft needing `composer::outer_height(draft_rows)`:
///
/// | budget           | separator | badge | box            | telemetry |
/// |------------------|-----------|-------|----------------|-----------|
/// | ≥ box+3          | ✓         | ✓     | box (3..=8)    | ✓         |
/// | ≥ box+2          | ✓         | ✓     | box            | shed      |
/// | ≥ box+1          | ✓         | shed  | box            | shed      |
/// | ≥ 3              | shed      | shed  | box min 3      | shed      |
/// | 1–2              | shed      | shed  | bare 1-row ❯   | shed      |
///
/// The box keeps its full wanted height through the first three tiers;
/// `outer_height` never exceeds [`theme::COMPOSER_MAX_OUTER`], so a
/// terminal that fits the idle footer (≥ [`theme::FOOTER_ROWS`]) always
/// hosts a fully grown box. Below three rows the last usable pixel is the
/// borderless prompt line.
fn footer_geometry(area: Rect, draft_rows: usize) -> FooterGeometry {
    if area.height == 0 || area.width == 0 {
        return FooterGeometry {
            band: Rect::new(area.x, area.y, area.width, 0),
            badge: None,
            input: Rect::new(area.x, area.y, area.width, 0),
            telemetry: None,
        };
    }

    let want = composer::outer_height(draft_rows);
    let rows = area.y + area.height;

// Bordered tiers. The first candidate that fits wins; each sheds one
    // accessory off the full band. Every tier also reserves at least three
    // live rows so the approval float (title + 1 body row + hints) stays
    // readable — the composer bends before the modal does.
    let mut tier = None;
    for &(extra, sep, badge, tele) in &[
        (3u16, true, true, true),
        (2, true, true, false),
        (1, true, false, false),
        (0, false, false, false),
    ] {
        let band_h = want + extra;
        if usize::from(area.height) >= usize::from(band_h) + MIN_LIVE_ROWS {
            tier = Some((band_h, sep, badge, tele));
            break;
        }
    }
    if let Some((band_h, sep, badge, tele)) = tier {
        let band_top = rows.saturating_sub(band_h);
        let mut used = band_top;
        // The separator occupies the first row whenever any accessory rides.
        if sep {
            used += 1;
        }
        let badge_row = badge.then(|| {
            let row = used;
            used += 1;
            row
        });
        let input = Rect::new(area.x, used, area.width, want);
        used += want;
        let telemetry = tele.then_some(used);
        return FooterGeometry {
            band: Rect::new(area.x, band_top, area.width, band_h),
            badge: badge_row,
            input,
            telemetry,
        };
    }

    // Bare final tier: one prompt row, still aligned left with CONTENT_INSET.
    let band_top = rows.saturating_sub(1);
    FooterGeometry {
        band: Rect::new(area.x, band_top, area.width, 1),
        badge: None,
        input: Rect::new(area.x, band_top, area.width, 1),
        telemetry: None,
    }
}

fn draw_band(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let width = usize::from(area.width);
    if area.is_empty() {
        app.note_history_layout(width, 0);
        return;
    }
    let column_width = usize::from(inset_h(area).width);
    let overlay = if app.has_confirmation() {
        // The approval stays content-sized, capped by the live band.
        app.overlay_frame_for(usize::from(area.height), column_width)
    } else {
        // Pickers float at a proportional dialog size (v0.37 §4.2), clamped
        // into the live band on small screens.
        app.overlay_frame_for(
            usize::from(theme::picker_height(area.height)),
            usize::from(theme::picker_width(area.width)),
        )
    };
    if let Some(overlay) = overlay {
        draw_overlay(frame, area, overlay);
        app.note_history_layout(width, 0);
        return;
    }

    // The menu spans exactly the input box's width and anchors at its
    // left edge, so it sits flush over the composer box (v0.44 §4.1). The
    // rounded panel spends one row on each border plus the P5 header row
    // and its TRACK separator, so the visible-list capacity gives those
    // four rows away first.
    let band_column = inset_h(Rect::new(area.x, area.y, area.width, 1));
    let menu_capacity = usize::from(area.height.saturating_sub(4)).min(theme::MENU_MAX_ROWS);
    if menu_capacity == 0 {
        draw_remaining_band(frame, app, area);
        return;
    }
    let menu = app.command_menu_frame(usize::from(band_column.width), menu_capacity);
    let menu_height = u16::try_from(menu.as_ref().map_or(0, |menu| menu.rows.len() + 4))
        .unwrap_or(u16::MAX);

    let remaining = area.height.saturating_sub(menu_height);
    let remaining_area = Rect::new(area.x, area.y, area.width, remaining);
    let menu_area = Rect::new(
        band_column.x,
        area.bottom().saturating_sub(menu_height),
        band_column.width,
        menu_height,
    );

    if !remaining_area.is_empty() {
        draw_remaining_band(frame, app, remaining_area);
    } else {
        app.note_history_layout(width, 0);
    }
    draw_command_menu(frame, menu, menu_area);
}

/// The command-menu float (P5 §3): a rounded PANEL_BG panel spanning exactly
/// the input band's width, anchored at its left edge directly above the
/// composer box. A header row (`Slash Commands` / `Tab complete · ↑↓ select`)
/// with a TRACK rule sits under the top border; the selected row wears the
/// MENU_ACTIVE_BG tint with an orange edge bar and a `▶` marker.
fn draw_command_menu(
    frame: &mut ratatui::Frame<'_>,
    menu: Option<crate::app::state::CommandMenuFrame>,
    area: Rect,
) {
    let Some(menu) = menu else {
        return;
    };
    let width = menu.width + 2;
    let outer_w = u16::try_from(width).unwrap_or(u16::MAX).min(area.width);
    let outer_h = u16::try_from(menu.rows.len() + 4)
        .unwrap_or(u16::MAX)
        .min(area.height);
    if outer_w == 0 || outer_h <= 4 {
        return;
    }
    let panel = Rect::new(
        area.x,
        area.bottom().saturating_sub(outer_h),
        outer_w,
        outer_h,
    );
    let inner = paint_panel(frame, panel, None);
    if inner.is_empty() {
        return;
    }

    // Header row inside the top border, with its TRACK rule below.
    let header_row = Rect::new(inner.x, inner.y, inner.width, 1);
    let left_label = "Slash Commands";
    let right_label = "Tab complete · ↑↓ select";
    let left_w = text::width(left_label);
    let right_w = text::width(right_label);
    let mut header_spans = vec![Span::styled(
        left_label.to_owned(),
        theme::corner_meta(),
    )];
    if let Some(pad) = usize::from(inner.width).checked_sub(left_w + right_w)
        && pad > 0
    {
        header_spans.push(Span::raw(" ".repeat(pad)));
    }
    header_spans.push(Span::styled(right_label.to_owned(), theme::corner_meta()));
    frame.render_widget(
        Paragraph::new(Line::from(header_spans)),
        header_row,
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            "─".repeat(usize::from(inner.width)),
            theme::footer_rule(),
        )),
        Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 1),
    );

    // Text sits one padding cell inside the borders, below the header rule.
    let zone = Rect::new(
        inner.x + 1,
        inner.y.saturating_add(2),
        inner.width.saturating_sub(2),
        inner.height.saturating_sub(2),
    );

    for (index, row) in menu.rows.iter().enumerate() {
        let y = zone.y.saturating_add(index as u16);
        if index >= usize::from(zone.height) || y >= zone.bottom() {
            break;
        }
        let full_row = Rect::new(inner.x, y, inner.width, 1);
        let row_area = Rect::new(zone.x, y, zone.width, 1);
        if index == menu.selected {
            // Full-row tint, an orange edge bar in the padding column, and
            // the command name in orange bold beside the gray summary.
            frame.render_widget(Block::default().style(theme::menu_selected_row()), full_row);
            frame.render_widget(
                Paragraph::new(Line::styled(
                    theme::STATUS_BAR.to_owned(),
                    theme::menu_selected_row(),
                )),
                Rect::new(inner.x, y, 1, 1),
            );
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(row.usage.clone(), theme::menu_selected_row()),
                    Span::styled(row.summary.clone(), theme::meta()),
                ]))
                .style(theme::menu_selected_row()),
                row_area,
            );
        } else {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(row.usage.clone(), theme::primary()),
                    Span::styled(row.summary.clone(), theme::meta()),
                ])),
                row_area,
            );
        }
    }
}

/// Paints a rounded float panel (`╭ ╮ ╰ ╯ ─ │`) over the brand canvas
/// (v4.0 P5 §3/§4: floats now wear the solid PANEL_BG surface — the
/// transparent v0.44 floats were retired with the reskin), with an optional
/// title embedded into the top border (`╭─ Title ───╮`). Returns the inner
/// drawing area between the borders.
fn paint_panel(frame: &mut ratatui::Frame<'_>, area: Rect, title: Option<(&str, Style)>) -> Rect {
    // v4.0: the "clear" now means clear-to-brand-canvas, and the panel then
    // washes the float in its solid PANEL_BG.
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::panel_bg_color())),
        area,
    );
    let border = theme::panel_border();
    let width = usize::from(area.width);
    if width < 2 || area.height == 0 {
        return Rect::new(area.x, area.y, 0, 0);
    }

    let top_row = Rect::new(area.x, area.y, area.width, 1);
    let mut top = vec![Span::styled("╭", border)];
    match title {
        Some((text, style)) if width >= 7 => {
            // ╭ + "─ " + title + " " + dashes + ╮
            let fitted = text::truncate(text, width - 6);
            let dashes = width.saturating_sub(5 + text::width(&fitted));
            top.push(Span::styled("─ ", border));
            top.push(Span::styled(fitted, style));
            top.push(Span::styled(format!(" {}", "─".repeat(dashes)), border));
        }
        _ => top.push(Span::styled("─".repeat(width - 2), border)),
    }
    top.push(Span::styled("╮", border));
    frame.render_widget(Paragraph::new(Line::from(top)), top_row);

    if area.height >= 2 {
        let bottom_row = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("╰", border),
                Span::styled("─".repeat(width - 2), border),
                Span::styled("╯", border),
            ])),
            bottom_row,
        );
    }

    // Side rails run down every row between the corners.
    for y in area.y + 1..area.bottom().saturating_sub(1) {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("│", border),
                Span::raw(" ".repeat(width - 2)),
                Span::styled("│", border),
            ])),
            Rect::new(area.x, y, area.width, 1),
        );
    }

    Rect::new(
        area.x + 1,
        area.y + 1,
        area.width - 2,
        area.height.saturating_sub(2),
    )
}

/// The leftover band above the composer belongs to the transcript: history
/// gets all remaining rows. In-progress answer/think are the open last cell
/// in that list.
///
/// v4.1 layout policy (the v2.2 40%/80% stream anchors are retired): the
/// viewport is the whole band. Content lays out from the band top; once
/// it overflows, the follow-bottom slice naturally pushes new rows in
/// from the bottom edge.
fn draw_remaining_band(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let column = inset_h(area);
    let width = usize::from(column.width);

    let selection = app.clamped_selection();
    let slice = app.history_slice(width, usize::from(column.height));

    let painted = usize::from(column.height).min(slice.total_rows);
    let paint = Rect {
        height: u16::try_from(painted).unwrap_or(u16::MAX).min(column.height),
        ..column
    };

    app.note_transcript_layout(BandLayout::from_parts(
        paint.x,
        paint.y,
        paint.width,
        paint.height,
    ));
    app.note_band_height(column.height);

    if paint.is_empty() {
        paint_rail(frame, app, area, width, usize::from(column.height));
        return;
    }
    frame.render_widget(
        Paragraph::new(history::paint_slice(&slice, selection, app.browse_cursor(), width)),
        paint,
    );
    paint_rail(frame, app, area, width, usize::from(column.height));
}

/// The rail spans the transcript band (not the painted sub-area) so the
/// geometry stays stable while content hangs from its tail.
fn paint_rail(
    frame: &mut ratatui::Frame<'_>,
    app: &App,
    area: Rect,
    width: usize,
    view_height: usize,
) {
    // The rail lives just past the canvas edge (the reserved full-frame
    // column), not inside the narrowed band.
    let rail = Rect::new(area.x + area.width, area.y, 1, area.height);
    let (total, offset) = app.scrollbar_metrics(width, view_height);
    scrollbar::paint(frame, rail, total, offset, app.scrollbar_active());
}

/// The v4.0 P3 §8 interception-confirmation box: the dark CONFIRM_BG
/// surface with a red border whose top edge runs the dashed `╌` rail, and
/// the embedded title painted red bold. Returns the inner text zone.
fn paint_confirm_panel(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
) -> Rect {
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::confirm_bg_color())),
        area,
    );
    let border = theme::err();
    let width = usize::from(area.width);
    if width < 2 || area.height == 0 {
        return Rect::new(area.x, area.y, 0, 0);
    }

    let top_row = Rect::new(area.x, area.y, area.width, 1);
    let fitted = text::truncate(title, width.saturating_sub(6));
    let dashes = width.saturating_sub(5 + text::width(&fitted));
    let mut top = vec![
        Span::styled("╭", border),
        Span::styled("╌ ", border),
        Span::styled(fitted, border.add_modifier(Modifier::BOLD)),
    ];
    if dashes > 0 {
        top.push(Span::styled(format!(" {}", "╌".repeat(dashes)), border));
    }
    top.push(Span::styled("╮", border));
    frame.render_widget(Paragraph::new(Line::from(top)), top_row);

    if area.height >= 2 {
        let bottom_row = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("╰", border),
                Span::styled("─".repeat(width - 2), border),
                Span::styled("╯", border),
            ])),
            bottom_row,
        );
    }
    for y in area.y + 1..area.bottom().saturating_sub(1) {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("│", border),
                Span::raw(" ".repeat(width - 2)),
                Span::styled("│", border),
            ])),
            Rect::new(area.x, y, area.width, 1),
        );
    }

    Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

/// Picker and approval floats (v0.44 §4.2): rounded dialogs centered in the
/// live band on the native terminal background, title embedded into the top
/// border, footer key hints on the last inner row. Pickers keep the fixed
/// dialog size (short lists pad with blanks); selected rows fill their full
/// inner width with the accent tint.
fn draw_overlay(frame: &mut ratatui::Frame<'_>, area: Rect, overlay: OverlayFrame) {
    use crate::app::overlay::OverlayTone;
    let outer_w = u16::try_from(overlay.width + 2)
        .unwrap_or(u16::MAX)
        .min(area.width);
    let outer_h = u16::try_from(overlay.body.len() + 3) // body + footer + borders
        .unwrap_or(u16::MAX)
        .min(area.height);
    if outer_w == 0 || outer_h <= 2 {
        return;
    }
    let x = area.x + (area.width - outer_w) / 2;
    let y = area.y + (area.height - outer_h) / 2;
    let panel = Rect::new(x, y, outer_w, outer_h);

    let inner = match overlay.tone {
        OverlayTone::Confirm => paint_confirm_panel(frame, panel, &overlay.title),
        OverlayTone::Normal => paint_panel(frame, panel, Some((&overlay.title, theme::primary()))),
    };
    if inner.is_empty() {
        return;
    }
    let zone = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );

    // Footer occupies the last inner row; body rows sit above it.
    let footer_row = zone.bottom().saturating_sub(1);
    let footer = match overlay.tone {
        // `[Enter/y] allow · [n] deny`: the allow key is green, the deny
        // key red, the separator dim (v4.0 P3 §8).
        OverlayTone::Confirm => {
            let mut spans = Vec::new();
            match overlay.footer.split_once(" · ") {
                Some((allow, deny)) => {
                    spans.push(Span::styled(allow, theme::ok()));
                    spans.push(Span::styled(" · ", theme::meta()));
                    spans.push(Span::styled(deny, theme::err()));
                }
                None => spans.push(Span::styled(overlay.footer.clone(), theme::meta())),
            }
            Paragraph::new(Line::from(spans))
        }
        _ => Paragraph::new(Line::styled(overlay.footer.clone(), theme::meta())),
    };
    frame.render_widget(footer, Rect::new(zone.x, footer_row, zone.width, 1));

    for (index, line) in overlay.body.iter().enumerate() {
        let y = zone.y.saturating_add(index as u16);
        if y >= footer_row {
            break;
        }
        draw_overlay_line(frame, Rect::new(inner.x, y, inner.width, 1), line, &zone);
    }
}

/// Paints one overlay row: the text lives in the padded zone while the
/// selected-row tint fills the full inner width with an orange edge bar in
/// the padding column (P5 §4). Unselected rows sit bare on the PANEL_BG
/// float surface.
fn draw_overlay_line(
    frame: &mut ratatui::Frame<'_>,
    full_row: Rect,
    line: &OverlayLine,
    zone: &Rect,
) {
    use crate::app::overlay::OverlayRow;
    let width = usize::from(zone.width);

    if line.selected {
        frame.render_widget(Block::default().style(theme::menu_selected_row()), full_row);
        // Orange edge bar (1 column) in the row's padding column.
        frame.render_widget(
            Paragraph::new(Line::styled(
                theme::STATUS_BAR.to_owned(),
                theme::menu_selected_row(),
            )),
            Rect::new(full_row.x, full_row.y, 1, 1),
        );
    }
    let base = if line.selected {
        theme::menu_selected_row()
    } else {
        Style::default()
    };

    /// Appends one styled piece truncated against the remaining budget.
    fn piece(
        spans: &mut Vec<Span<'static>>,
        used: &mut usize,
        width: usize,
        text: String,
        style: ratatui::style::Style,
    ) {
        if *used >= width {
            return;
        }
        let fitted = text::truncate(&text, width - *used);
        *used += text::width(&fitted);
        spans.push(Span::styled(fitted, style));
    }

    let spans = match &line.row {
        OverlayRow::Text(value) => {
            vec![Span::styled(text::truncate(value, width), theme::primary())]
        }
        OverlayRow::Group(name) => vec![Span::styled(
            text::truncate(name, width),
            theme::corner_meta().add_modifier(Modifier::ITALIC),
        )],
        OverlayRow::Empty(message) => vec![Span::styled(
            text::truncate(message, width),
            theme::placeholder(),
        )],
        OverlayRow::Detail(detail) => {
            vec![Span::styled(text::truncate(detail, width), theme::meta())]
        }
        OverlayRow::Entry {
            marked,
            primary,
            tail,
        } if line.selected => {
            // P5 §4 selected row: `▶ ` marker then the entry in orange bold
            // over the tinted surface; the secondary meta keeps its gray.
            let mut spans = Vec::new();
            let mut used = 0usize;
            piece(&mut spans, &mut used, width, "▶ ".to_owned(), theme::accent());
            piece(
                &mut spans,
                &mut used,
                width,
                format!("{primary}{tail}"),
                theme::menu_selected_row(),
            );
            spans
        }
        OverlayRow::Entry {
            marked,
            primary,
            tail,
        } => {
            let mut spans = Vec::new();
            let mut used = 0usize;
            piece(
                &mut spans,
                &mut used,
                width,
                format!("{} ", if *marked { "•" } else { " " }),
                theme::accent(),
            );
            piece(
                &mut spans,
                &mut used,
                width,
                primary.clone(),
                theme::primary(),
            );
            piece(&mut spans, &mut used, width, tail.clone(), theme::meta());
            spans
        }
    };
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(base),
        Rect::new(zone.x, full_row.y, zone.width, 1),
    );
}

/// The v4.0 footer band (P2 §2): FOOTER_BG fill, TRACK separator rule on
/// top, the badge row (`● Ready │ model · effort`), the rounded input box
/// between them, and the telemetry row (`path │ ↑↓R C ctx`). All rows keep
/// [`theme::FOOTER_PAD`] columns of air to the band edges.
fn draw_footer(frame: &mut ratatui::Frame<'_>, app: &App, slots: FooterSlots) {
    if slots.band.is_empty() {
        return;
    }
    frame.render_widget(Block::default().style(theme::footer_fill()), slots.band);

    let band_column = Rect::new(
        slots.band.x + theme::FOOTER_PAD,
        slots.band.y,
        slots.band.width.saturating_sub(theme::FOOTER_PAD * 2),
        slots.band.height,
    );

    // TRACK separator rule on the band's very top row (P2 §2.1).
    if slots.badge.is_some() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "─".repeat(usize::from(slots.band.width)),
                theme::footer_rule(),
            )),
            Rect::new(slots.band.x, slots.band.y, slots.band.width, 1),
        );
    }
    if let Some(row) = slots.badge {
        draw_badge_row(frame, app, row, band_column);
    }
    if let Some(row) = slots.telemetry {
        draw_telemetry_row(frame, app, row, band_column);
    }
    draw_input_box(frame, app, slots.input);
}

/// Badge row, left side. Idle wears the green dot + `Ready`; an active run
/// replaces the dot with its phase spinner and pure elapsed timing
/// (`⠋ Thinking (4.2s)` — decision D2/D3/D8; no `esc cancel` tail, D11).
/// Browse mode appends the `(browse)` suffix (P5 §2.2).
fn draw_badge_row(frame: &mut ratatui::Frame<'_>, app: &App, row: u16, column: Rect) {
    let width = usize::from(column.width);
    let browse = app.in_browse_mode();
    let suffix = if browse { " (browse)" } else { "" };
    // The corner's budget shrinks by the suffix so the right-hand model
    // corner never collides with it.
    let budget = width.saturating_sub(text::width(suffix));
    let left = app.run_state_corner(budget).map(|state| StateBadge {
        spinner: state.spinner,
        word: state.word,
        timing: state.timing,
        warning: state.warning,
    });
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
    match left {
        Some(badge) => {
            let word = badge_word(&badge);
            if badge.warning {
                spans.push(Span::styled(badge.spinner.clone(), theme::err()));
            } else {
                spans.push(Span::styled(
                    badge.spinner,
                    spinner_style(&word),
                ));
            }
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                word,
                if badge.warning {
                    theme::err()
                } else {
                    theme::primary()
                },
            ));
            if !badge.timing.is_empty() {
                // `corner` yields `{elapsed} · esc cancel`; v4.0 keeps only
                // the pure timer inside parentheses.
                spans.push(Span::styled(
                    format!(" ({})", badge.timing),
                    theme::corner_meta(),
                ));
            }
        }
        None => {
            spans.push(Span::styled(
                theme::STATUS_DOT.to_owned(),
                theme::status_dot_idle(),
            ));
            spans.push(Span::raw(" "));
            spans.push(Span::styled("Ready", theme::primary()));
        }
    }
    if browse {
        spans.push(Span::styled(suffix.to_owned(), theme::corner_meta()));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(column.x, row, column.width, 1),
    );

    draw_model_corner(frame, app, row, column);
}

/// Spinner color per D8 mapping: orange braille for Thinking-family words,
/// yellow horizontal for Running/Compacting.
fn spinner_style(word: &str) -> Style {
    if word.starts_with("Running") || word.starts_with("Compacting") || word == "Approval…" {
        theme::warn()
    } else {
        theme::accent()
    }
}

/// The badge word without the v3 ellipsis-suffix semantics: keep the corner
/// machine's word but drop its trailing `…` for the clean v4.0 form.
fn badge_word(badge: &StateBadge) -> String {
    badge.word.trim_end_matches('…').to_owned()
}

struct StateBadge {
    spinner: String,
    word: String,
    timing: String,
    warning: bool,
}

/// Badge row, right side: `{model} · {effort}` with BLUE+BOLD and
/// YELLOW+BOLD tokens around a DARK_GRAY separator. Provider annotation is
/// retired with the v3 dashboard.
fn draw_model_corner(frame: &mut ratatui::Frame<'_>, app: &App, row: u16, column: Rect) {
    let Some(corner) = app.status.model_corner_for(usize::from(column.width)) else {
        return;
    };
    let mut width = text::width(&corner.model);
    let mut spans = vec![Span::styled(corner.model.clone(), theme::model_name())];
    if let Some(effort) = &corner.effort {
        width += text::width(effort) + 3;
        spans.push(Span::styled(" · ".to_owned(), theme::corner_meta()));
        spans.push(Span::styled(effort.clone(), theme::model_effort()));
    }
    paint_right_aligned(frame, spans, width, row, column);
}

/// Telemetry row: workspace path left (green), usage right. The usage line
/// renders as styled segments — labels gray, values yellow bold, `-`
/// placeholders dark gray.
fn draw_telemetry_row(frame: &mut ratatui::Frame<'_>, app: &App, row: u16, column: Rect) {
    let width = usize::from(column.width);
    let (path, usage) = app.status.bottom_corners_for(width);

    if !path.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(path.clone(), theme::workspace_path())),
            Rect::new(
                column.x,
                row,
                u16::try_from(text::width(&path)).unwrap_or(1),
                1,
            ),
        );
    }
    if !usage.is_empty() {
        let spans = telemetry_spans(&usage);
        let total: usize = spans.iter().map(|span| text::width(&span.content)).sum();
        paint_right_aligned(frame, spans, total, row, column);
    }
}

/// Splits `↑11k ↓4.8k R14k C51.3% 2.2%/500k` into identifier/value spans:
/// leading letter or arrow = label (gray); the rest of the token = value
/// (yellow bold); separators between groups stay plain.
fn telemetry_spans(usage: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, group) in usage.split(' ').filter(|g| !g.is_empty()).enumerate() {
        if index > 0 {
            spans.push(Span::raw("  ".to_owned()));
        }
        let split_at = group
            .char_indices()
            .find(|(position, ch)| {
                *position > 0 && !matches!(ch, '↑' | '↓' | 'R' | 'C')
            })
            .map(|(position, _)| position)
            .unwrap_or(group.len());
        let (label, value) = group.split_at(split_at.min(group.len()));
        spans.push(Span::styled(label.to_owned(), theme::telemetry_label()));
        spans.push(Span::styled(value.to_owned(), telemetry_value_style(value)));
    }
    spans
}

/// Placeholder dashes wear the quiet tone; real numbers go yellow+bold;
/// the `ctx%/window` tail splits at `/`: value yellow, window dim.
fn telemetry_value_style(value: &str) -> Style {
    if value == "-" || !value.chars().any(char::is_numeric) {
        return theme::corner_meta();
    }
    theme::telemetry_value()
}

/// The rounded input box (P2 §3.1): `╭─╮ / │ ❯ draft / ╰─╯`, transparent
/// over the FOOTER_BG band. Inner height follows the rect the layout pass
/// handed in; drafts taller than it scroll internally with the `[L]` marker.
fn draw_input_box(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }

    // Bare tier: no borders fit; one prompt line still renders.
    if area.height < theme::COMPOSER_MIN_OUTER {
        draw_bare_prompt_line(frame, app, area);
        return;
    }

    let border = theme::panel_border();
    let width = usize::from(area.width);
    let top_row = Rect::new(area.x, area.y, area.width, 1);
    let bottom_row = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("╭", border),
            Span::styled("─".repeat(width.saturating_sub(2)), border),
            Span::styled("╮", border),
        ])),
        top_row,
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("╰", border),
            Span::styled("─".repeat(width.saturating_sub(2)), border),
            Span::styled("╯", border),
        ])),
        bottom_row,
    );

    // Side rails between the corners.
    for y in area.y + 1..area.bottom().saturating_sub(1) {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("│", border),
                Span::raw(" ".repeat(width.saturating_sub(2))),
                Span::styled("│", border),
            ])),
            Rect::new(area.x, y, area.width, 1),
        );
    }

    let inner = Rect::new(
        area.x + 1,
        area.y + 1,
        area.width - 2,
        area.height - 2,
    );
    draw_input_inner(frame, app, inner);
}

/// Draws the draft/prompt rows inside a bordered box; `[L{n}/{total}]`
/// rides the bottom border's right end while the internal scroller moves.
fn draw_input_inner(frame: &mut ratatui::Frame<'_>, app: &App, inner: Rect) {
    let prompt_width = usize::from(inner.width).saturating_sub(3);
    let view = composer::viewport(&app.input, prompt_width.max(1), usize::from(inner.height));
    let busy_activity = app.has_confirmation() || app.run_state_active();
    let prompt_style = if busy_activity {
        theme::prompt_busy()
    } else {
        theme::prompt_ready()
    };

    if !inner.is_empty() {
        // Prompt glyph column then draft rows.
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(" "),
                Span::styled(theme::PROMPT.to_owned(), prompt_style),
                Span::raw(" "),
            ])),
            Rect::new(inner.x, inner.y, 3.min(inner.width), 1),
        );
        let text_area = Rect::new(
            inner.x.saturating_add(3),
            inner.y,
            inner.width.saturating_sub(3),
            inner.height,
        );
        if !text_area.is_empty() {
            frame.render_widget(
                Paragraph::new(composer::styled_rows_painted(&view)),
                text_area,
            );
        }
        if view.empty && text_area.width >= 1 {
            // The centered empty-draft row carries the quiet placeholder.
            let placeholder_row = Rect::new(
                inner.x + 3,
                inner.y + u16::try_from(view.cursor_y).unwrap_or(0),
                inner.width.saturating_sub(3),
                1,
            );
            if placeholder_row.y < inner.bottom() {
                frame.render_widget(
                    Paragraph::new(Line::styled(
                        "Ask anything",
                        theme::placeholder(),
                    )),
                    placeholder_row,
                );
            }
        }
        if app.input_focused() {
            let cursor_x = inner
                .x
                .saturating_add(3)
                .saturating_add(u16::try_from(view.cursor_x).unwrap_or(0));
            let cursor_y = inner
                .y
                .saturating_add(u16::try_from(view.cursor_y).unwrap_or(0));
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }

    // Internal-scroll position label on the bottom border's right side.
    if view.scrolls(usize::from(inner.height)) {
        let label = format!(
            "[L{}/{}]",
            view.first_visual_row.saturating_add(1),
            view.total_visual_rows
        );
        let label_width = text::width(&label);
        if usize::from(inner.width.saturating_sub(1)) >= label_width {
            let x = inner.right().saturating_sub(u16::try_from(label_width).unwrap_or(1));
            frame.render_widget(
                Paragraph::new(Line::styled(label, theme::panel_border())),
                Rect::new(x, inner.bottom(), u16::try_from(label_width).unwrap_or(1), 1),
            );
        }
    }
}

/// Borderless single-row input (short-screen final tier): just `❯ draft`.
fn draw_bare_prompt_line(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let view = composer::viewport(
        &app.input,
        usize::from(area.width).saturating_sub(2).max(1),
        1,
    );
    let busy_activity = app.has_confirmation() || app.run_state_active();
    let prompt_style = if busy_activity {
        theme::prompt_busy()
    } else {
        theme::prompt_ready()
    };
    frame.render_widget(
        Paragraph::new(Line::styled(theme::PROMPT.to_owned(), prompt_style)),
        Rect::new(area.x, area.y, 1.min(area.width), 1),
    );
    if area.width > 2 {
        let _ = 2;
        frame.render_widget(
            Paragraph::new(composer::styled_rows_painted(&view)),
            Rect::new(
                area.x.saturating_add(2),
                area.y,
                area.width.saturating_sub(2),
                1,
            ),
        );
    }
    if app.input_focused() {
        let cursor_x = area.x.saturating_add(2 + u16::try_from(view.cursor_x).unwrap_or(0));
        frame.set_cursor_position((cursor_x, area.y));
    }
}

/// Right-aligned corner content inside the band's content column.
fn paint_right_aligned(
    frame: &mut ratatui::Frame<'_>,
    spans: Vec<Span<'static>>,
    width: usize,
    row: u16,
    column: Rect,
) {
    if width == 0 || usize::from(column.width) < width {
        return;
    }
    let x = column.x + u16::try_from(usize::from(column.width) - width).unwrap_or(0);
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(x, row, u16::try_from(width).unwrap_or(1), 1),
    );
}

#[cfg(test)]
pub(crate) fn composer_y(height: u16) -> u16 {
    panel_areas(Rect::new(0, 0, 80, height), 0).composer.y
}

#[cfg(test)]
mod tests {
    use philo_agent_service::{FrontendOperationEvent, FrontendTokenUsage};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::app::action::Action;
    use crate::app::overlay::PickerEntry;
    use crate::app::status::StatusData;
    use crate::app::transcript::{InfoLevel, LineKind, TranscriptLine};
    use crate::render::{CONTENT_INSET, inset_h};

    use super::*;

    fn app() -> App {
        App::new(
            StatusData::new("gpt-test", "session-中文", InfoLevel::Default),
            true,
        )
    }

    fn render(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw(frame, app, false))
            .expect("draw");
        terminal
            .backend()
            .buffer()
            .content
            .chunks(usize::from(width))
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn shows(rendered: &str, text: &str) -> bool {
        rendered
            .lines()
            .any(|line| line.trim().trim_end_matches(['│', '█']).trim() == text)
    }

    /// v4.0 P2: the whole canvas paints BASE_BG; the transcript paints on
    /// top and the rail carries track/thumb glyphs for overflowing history.
    #[test]
    fn the_canvas_fills_base_bg_and_the_rail_paints_scrollbar() {
        let mut app = app();
        app.cells.push_closed((0..60).map(|i| TranscriptLine {
            kind: LineKind::Meta,
            text: format!("row-{i}"),
            tone: crate::app::transcript::Tone::Plain,
            header: None,
            body: None,
        }));

        let width = 80u16;
        let height = VIEWPORT_HEIGHT;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, &app, false)).expect("draw");
        let buffer = terminal.backend().buffer();
        let rail_x = usize::from(width) - 1;

        let mut thumb_rows = Vec::new();
        let mut track_rows = Vec::new();
        // The rail spans the transcript band only; the footer below stays quiet.
        let band_end = usize::from(height) - 6;
        for row in 0..band_end {
            match buffer.content[row * usize::from(width) + rail_x].symbol() {
                "█" => thumb_rows.push(row),
                "│" => track_rows.push(row),
                other => panic!("rail row {row} wears {other:?}"),
            }
        }
        assert!(
            !thumb_rows.is_empty() && !track_rows.is_empty(),
            "overflowing history shows thumb ({thumb_rows:?}) inside track ({track_rows:?})"
        );

        // Non-overflowing content keeps the quiet full-track rail.
        let idle_app = App::new(
            StatusData::new("gpt-test", "session-中文", InfoLevel::Default),
            true,
        );
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &idle_app, false))
            .expect("draw");
        let idle_buffer = terminal.backend().buffer();
        for row in 0..band_end {
            assert_eq!(
                idle_buffer.content[row * usize::from(width) + rail_x].symbol(),
                "│",
                "an empty transcript leaves the bare track"
            );
        }
    }


    #[test]
    fn composer_geometry_is_independent_of_transient_state() {
        let baseline = panel_areas(Rect::new(0, 0, 80, VIEWPORT_HEIGHT), 0);
        assert_eq!(baseline.composer.height, theme::COMPOSER_MIN_OUTER);
        assert_eq!(
            baseline.live.height,
            VIEWPORT_HEIGHT - theme::FOOTER_ROWS.min(VIEWPORT_HEIGHT)
        );
        for height in [MIN_SUPPORTED_HEIGHT, VIEWPORT_HEIGHT, 20] {
            let areas = panel_areas(Rect::new(0, 0, 80, height), 0);
            assert_eq!(areas.live.bottom(), areas.footer.y);
            assert_eq!(areas.footer.bottom(), height, "band owns the floor");
            assert!(
                !areas.composer.is_empty(),
                "the prompt survives every supported height"
            );
        }
    }

/// P2 §3.2 ladder: accessories shed telemetry → badge → separator while
    /// every bordered tier keeps at least MIN_LIVE_ROWS of transcript above
    /// it; the box never shrinks below three rows until borders themselves
    /// stop fitting.
    #[test]
    fn short_screens_shed_accessories_then_go_bare() {
        let want = |rows: u16, drafts: usize| {
            let g = footer_geometry(Rect::new(0, 0, 80, rows), drafts);
            (g.badge.is_some(), g.telemetry.is_some(), g.input)
        };

        // Full band at ≥9 rows (3 accessory rows + 3 box + 3 live).
        let (badge, tele, input) = want(9, 0);
        assert!(badge && tele && input.height == theme::COMPOSER_MIN_OUTER);

        // Six rows: live reservation wins over the full band — the box
        // keeps its three rows but neither accessory rides.
        let (badge, tele, input) = want(theme::FOOTER_ROWS, 0);
        assert!(!badge && !tele && input.height == theme::COMPOSER_MIN_OUTER);

        // Five and four: bordered box still fits (3 box + 3 live = 6 > 5,
        // so no —) the bare prompt takes over below six.
        let (badge, tele, input) = want(5, 0);
        assert!(!badge && !tele && input.height == 1);

        let (badge, tele, input) = want(4, 0);
        assert!(!badge && !tele && input.height == 1);

        // Three to two: bare prompt only.
        let (badge, tele, input) = want(2, 0);
        assert!(!badge && !tele && input.height == 1);
        let (badge, _, input) = want(MIN_SUPPORTED_HEIGHT.saturating_sub(1), 0);
        assert!(!badge && input.height == 2 || input.height >= 1);

        assert!(footer_geometry(Rect::new(0, 0, 80, 1), 0).input.height <= 1);
    }

    #[test]
    fn the_idle_screen_is_transcript_plus_footer_only() {
        let rendered = render(&app(), 80, VIEWPORT_HEIGHT);
        let rows: Vec<&str> = rendered.lines().collect();

        // Band layout top→bottom: separator rule, badge, box (3), telemetry.
        assert_eq!(
            rows[VIEWPORT_HEIGHT as usize - 6],
            "─".repeat(79),
            "the TRACK rule rides band top"
        );
        assert!(
            rows[VIEWPORT_HEIGHT as usize - 5].contains("● Ready"),
            "idle badge: {:?}",
            rows[VIEWPORT_HEIGHT as usize - 5]
        );
        assert!(
            rows[VIEWPORT_HEIGHT as usize - 4].starts_with("╭")
                && rows[VIEWPORT_HEIGHT as usize - 4].ends_with("╮"),
            "rounded box opens: {:?}",
            rows[VIEWPORT_HEIGHT as usize - 4]
        );
        assert!(
            rows[VIEWPORT_HEIGHT as usize - 3].contains('❯'),
            "prompt glyph rides the inner row: {:?}",
            rows[VIEWPORT_HEIGHT as usize - 3]
        );
        assert!(
            rows[VIEWPORT_HEIGHT as usize - 2].starts_with("╰")
                && rows[VIEWPORT_HEIGHT as usize - 2].ends_with("╯"),
            "rounded box closes"
        );
        assert!(
            rows[VIEWPORT_HEIGHT as usize - 1].contains(r"↑-  ↓-  R-  C-  -/-"),
            "usage hugs the band's right pad: {:?}",
            rows[VIEWPORT_HEIGHT as usize - 1]
        );
    }

    #[test]
    fn composer_baseline_does_not_move_for_activity_or_popovers() {
        let expected = usize::from(composer_y(VIEWPORT_HEIGHT));

        let mut activity = app();
        activity.set_busy(true);
        activity.on_operation_event(&FrontendOperationEvent::ToolBatchRequested {
            tool_batch_id: "batch".to_owned(),
            call_count: 1,
        });
        assert_eq!(usize::from(composer_y(VIEWPORT_HEIGHT)), expected);

        let mut completion = app();
        completion.on_paste("/s");
        completion.on_action(Action::Complete);
        assert_eq!(usize::from(composer_y(VIEWPORT_HEIGHT)), expected);

        let mut picker = app();
        picker.open_picker(vec![
            PickerEntry::untitled("one"),
            PickerEntry::untitled("two"),
        ]);
        assert_eq!(usize::from(composer_y(VIEWPORT_HEIGHT)), expected);

        let mut approval = app();
        approval.sync_confirmation(Some((1, "write file".to_owned(), "src/main.rs".to_owned())));
        assert_eq!(usize::from(composer_y(VIEWPORT_HEIGHT)), expected);
    }

    #[test]
    fn a_multi_row_draft_grows_the_box_and_clips_at_eight() {
        let mut app = app();
        for index in 0..7 {
            if index > 0 {
                app.on_action(Action::InsertNewline);
            }
            for ch in format!("line-{index}").chars() {
                app.on_action(Action::InsertChar(ch));
            }
        }
        // Seven logical rows → outer height 8... capped: min(7+2, 8) → 8.
        let geometry = footer_geometry(Rect::new(0, 0, 80, 40), draft_wrapped_rows(&app, 74));
        assert_eq!(geometry.band.height, theme::COMPOSER_MAX_OUTER + 3);
        assert_eq!(geometry.input.height, theme::COMPOSER_MAX_OUTER);

        // One extra row pushes into internal scrolling with the label.
        app.on_action(Action::InsertNewline);
        for ch in "tail".chars() {
            app.on_action(Action::InsertChar(ch));
        }
        let rendered = render(&app, 80, 30);
        assert!(
            rendered.contains("[L"),
            "internal scroll marker shows once over cap: {rendered}"
        );

        crate::tests::assert_tui_snapshot!("m15_composer_growth", rendered);
    }

    #[test]
    fn the_command_menu_paints_above_the_composer() {
        let mut app = app();
        for ch in "/s".chars() {
            app.on_action(Action::InsertChar(ch));
        }
        let rendered = render(&app, 80, VIEWPORT_HEIGHT);
        assert!(
            rendered
                .lines()
                .any(|row| row.contains("/sessions")),
            "menu candidates ride above the box: {rendered}"
        );
        crate::tests::assert_tui_snapshot!("m18_command_menu", rendered);
    }

    /// Design §3.6: the auto menu floats as a rounded panel anchored at the
    /// content column's left edge, directly above the composer box.
    #[test]
    fn m6_command_menu_float_panel() {
        let mut app = app();
        for ch in "/s".chars() {
            app.on_action(Action::InsertChar(ch));
        }
        let rendered = render(&app, 80, 24);
        assert!(
            rendered.contains('╭') && rendered.contains('╰'),
            "the menu wears a rounded border: {rendered}"
        );
        assert!(
            !rendered.lines().any(|line| line.starts_with("› ")),
            "the panel no longer hugs column 0: {rendered}"
        );
        crate::tests::assert_tui_snapshot!("m6_command_menu", rendered);
    }

    /// Design §3.7: the session picker floats as a centered dialog with an
    /// embedded title, relative times right-aligned, `•` on the current
    /// session, and preview details under the highlight.
    #[test]
    fn m6_session_picker_centered_dialog() {
        use crate::app::overlay::Preview;

        let mut app = app();
        app.open_picker(vec![
            PickerEntry {
                id: "s-flaky".to_owned(),
                primary: "fix the flaky test on windows".to_owned(),
                secondary: "now".to_owned(),
                group: String::new(),
                marked: false,
                tiers: Vec::new(),
            },
            PickerEntry::untitled("s-auth").with_secondary("3m"),
            PickerEntry {
                id: "s-image".to_owned(),
                primary: "s-image".to_owned(),
                secondary: "2d".to_owned(),
                group: String::new(),
                marked: true,
                tiers: Vec::new(),
            },
        ]);
        if let Some(id) = app.claim_preview() {
            app.set_preview(
                &id,
                Preview::Ready(vec![
                    "last: refactor the auth middleware to…".to_owned(),
                    "one file".to_owned(),
                ]),
            );
        }

        let rendered = render(&app, 80, 24);
        let top_border = rendered
            .lines()
            .find(|row| row.contains('╭'))
            .expect("the picker floats as a rounded dialog");
        let left = top_border
            .chars()
            .position(|glyph| glyph == '╭')
            .expect("╭");
        let right = top_border
            .chars()
            .position(|glyph| glyph == '╮')
            .expect("╮");
        assert!(
            left > usize::from(theme::CONTENT_INSET),
            "the dialog floats clear of the left edge: {rendered}"
        );
        assert!(
            right < 79,
            "the dialog floats clear of the right edge: {rendered}"
        );
        assert!(
            rendered
                .lines()
                .any(|row| row.contains('•') && row.contains("s-image")),
            "the current session wears the accent dot: {rendered}"
        );
        crate::tests::assert_tui_snapshot!("m6_session_picker", rendered);
    }

    /// Design §3.8: the model picker groups providers as dim small caps and
    /// flags the active model with the `current` word.
    #[test]
    fn m6_model_picker_grouped_by_provider() {
        let mut app = app();
        app.open_model_picker(vec![
            PickerEntry {
                id: "anthropic/claude-sonnet-4-5".to_owned(),
                primary: "anthropic/claude-sonnet-4-5".to_owned(),
                secondary: "current".to_owned(),
                group: "anthropic".to_owned(),
                marked: true,
                tiers: Vec::new(),
            },
            PickerEntry {
                id: "anthropic/claude-opus-4-5".to_owned(),
                primary: "anthropic/claude-opus-4-5".to_owned(),
                secondary: String::new(),
                group: "anthropic".to_owned(),
                marked: false,
                tiers: Vec::new(),
            },
            PickerEntry {
                id: "openai/gpt-5.2".to_owned(),
                primary: "openai/gpt-5.2".to_owned(),
                secondary: String::new(),
                group: "openai".to_owned(),
                marked: false,
                tiers: Vec::new(),
            },
        ]);

        let rendered = render(&app, 80, 24);
        assert!(
            rendered
                .lines()
                .any(|row| row.contains("ANTHROPIC") && row.contains('│')),
            "provider heads sit inside the bordered dialog: {rendered}"
        );
        assert!(
            rendered.lines().any(|row| row.contains("current")),
            "the active model is flagged in words: {rendered}"
        );
        crate::tests::assert_tui_snapshot!("m6_model_picker", rendered);
    }

/// Design §3.9 / v4.0 P3 §8: the approval prompt is a centered red
    /// dialog on the dark CONFIRM_BG with a dashed top border; the footer
    /// below keeps its draft and cursor position.
    #[test]
    fn m6_confirmation_centered_dialog() {
        let mut app = app();
        app.on_paste("draft 中文");
        app.sync_confirmation(Some((
            7,
            "write workspace file".to_owned(),
            "path  src/auth/session.rs".to_owned(),
        )));

        let rendered = render(&app, 80, 24);
        assert!(
            rendered
                .lines()
                .any(|row| row.trim_start().starts_with("╭╌ Approval required")),
            "the title embeds into the dashed red top border: {rendered}"
        );
        assert!(
            rendered
                .lines()
                .any(|row| row.contains("[Enter/y] allow · [n] deny")),
            "the key hints sit on the footer row: {rendered}"
        );
        assert!(
            rendered.lines().any(|row| row.contains("draft")),
            "the draft survives beneath the dialog: {rendered}"
        );
        crate::tests::assert_tui_snapshot!("m6_confirmation", rendered);
    }

    fn dashboard_app() -> App {
        let mut status = StatusData::new("gpt-5.2", "session-中文", InfoLevel::Default);
        status.provider = Some("openai".to_owned());
        status.effort = Some("high".to_owned());
        status.workspace_root = r"D:\Code\Zed\Year2026\Jul0706\Pi".to_owned();
        status.usage = Some(FrontendTokenUsage {
            input_tokens: Some(11_000),
            output_tokens: Some(4_800),
            cache_read_tokens: Some(5_640),
            reasoning_tokens: Some(14_000),
            ..FrontendTokenUsage::default()
        });
        status.context_window = Some(500_000);
        App::new(status, true)
    }

    fn freeze_turn(app: &mut App, secs: u64) {
        app.run_state_mut()
            .freeze_elapsed(std::time::Duration::from_secs(secs));
    }

    /// D2/D3/D8/D11: the state badge replaces the old corner word — pure
    /// timer in parentheses, no `esc cancel` tail; the model·effort block
    /// sits right-aligned on the same row.
    #[test]
    fn the_state_badge_replaces_the_old_corner_word() {
        let mut app = dashboard_app();
        app.set_busy(true);
        app.on_operation_event(&FrontendOperationEvent::TextDelta {
            delta: "streaming".to_owned(),
        });
        app.flush_stream();
        freeze_turn(&mut app, 42);

        let rendered = render(&app, 80, VIEWPORT_HEIGHT);
        let badge_row = rendered
            .lines()
            .find(|line| line.contains("Writing"))
            .expect("writing badge");
        assert!(
            badge_row.contains("(42s)"),
            "pure timer inside parentheses: {badge_row:?}"
        );
        assert!(
            !badge_row.contains("esc cancel"),
            "D11 retires the esc tail: {badge_row:?}"
        );
        assert!(
            badge_row.contains("gpt-5.2") && badge_row.contains("high"),
            "model · effort share the badge row: {badge_row:?}"
        );
        assert!(
            !badge_row.contains("(openai)"),
            "provider annotation is retired: {badge_row:?}"
        );
        crate::tests::assert_tui_snapshot!("m3_writing_state", rendered);

        // Settlement returns the green Ready dot.
        app.on_operation_event(&FrontendOperationEvent::OperationSettled {
            operation_id: "op".to_owned(),
            session_id: "s".to_owned(),
            status: "Succeeded".to_owned(),
            durability: "Confirmed".to_owned(),
            session_revision: philo_agent_service::SettlementRevision::Unchanged,
        });
        let settled_rows = render(&app, 80, VIEWPORT_HEIGHT);
        assert!(
            settled_rows.contains("● Ready"),
            "settlement restores the idle badge: {settled_rows}"
        );
        assert!(!settled_rows.contains("Writing"));
    }

    #[test]
    fn approval_overlays_the_word_and_reveals_it_again() {
        let mut app = dashboard_app();
        app.set_busy(true);
        freeze_turn(&mut app, 12);
        app.sync_confirmation(Some((
            1,
            "write workspace file".to_owned(),
            "src/main.rs".to_owned(),
        )));

        let rendered = render(&app, 80, VIEWPORT_HEIGHT);
        let badge_row = rendered
            .lines()
            .find(|line| line.contains("Approval") && line.contains("(12s)"))
            .expect("approval flag owns the badge");
        assert!(
            !badge_row.contains("esc cancel"),
            "D11 retires the esc tail: {badge_row:?}"
        );
        assert!(
            !badge_row.contains("esc cancel"),
            "D11 retires the esc tail: {badge_row:?}"
        );
        crate::tests::assert_tui_snapshot!("m3_approval_overlay", rendered);

        app.sync_confirmation(None);
        let revealed = render(&app, 80, VIEWPORT_HEIGHT);
        assert!(
            revealed.lines().any(|line| line.contains("Waiting (12s)")),
            "resolving reveals the underlying phase: {revealed}"
        );
    }

    #[test]
    fn browse_mode_appends_the_badge_suffix_and_drops_it_on_exit() {
        let mut app = app();
        app.cells.push_closed((0..25).map(|i| TranscriptLine {
            kind: LineKind::Meta,
            text: format!("row-{i}"),
            tone: crate::app::transcript::Tone::Plain,
            header: None,
            body: None,
        }));
        app.note_history_layout(80, usize::from(VIEWPORT_HEIGHT) - 4);

        let idle = render(&app, 80, VIEWPORT_HEIGHT);
        let idle_badge = idle
            .lines()
            .find(|line| line.contains("Ready"))
            .expect("idle badge row");
        assert!(!idle_badge.contains("browse"), "{idle_badge:?}");

        app.on_action(Action::EnterBrowse);
        let browsing = render(&app, 80, VIEWPORT_HEIGHT);
        let badge = browsing
            .lines()
            .find(|line| line.contains("Ready"))
            .expect("badge row while browsing");
        assert!(
            badge.contains("(browse)"),
            "browse mode appends the suffix: {badge:?}"
        );

        app.on_action(Action::ExitBrowse);
        let exited = render(&app, 80, VIEWPORT_HEIGHT);
        let badge = exited
            .lines()
            .find(|line| line.contains("Ready"))
            .expect("badge row after exiting");
        assert!(!badge.contains("browse"), "{badge:?}");
    }

    #[test]
    fn the_idle_dashboard_fills_badge_and_telemetry_rows() {
        let rendered = render(&dashboard_app(), 80, VIEWPORT_HEIGHT);
        let rows: Vec<&str> = rendered.lines().collect();

        assert!(
            rows.iter().any(|row| row.contains("● Ready")
                && row.ends_with("gpt-5.2 · high")),
            "dot + Ready left, model·effort right: {rendered}"
        );
assert!(
            rows.iter().any(|row| {
                row.contains(r"D:\Code\Zed\Year2026\Jul0706\Pi")
                    && row.trim_end().ends_with("2.2%/500k")
            }),
            "root left, usage right on the telemetry row: {rendered}"
        );

        crate::tests::assert_tui_snapshot!("m2_idle_dashboard", rendered);
    }

    #[test]
    fn the_bottom_row_degrades_path_first_then_c_then_r() {
        // 40 columns: compact path, then usage loses C% and R while arrows
        // and ctx/window survive.
        let rendered = render(&dashboard_app(), MIN_SUPPORTED_WIDTH, VIEWPORT_HEIGHT);
        assert!(
            rendered.contains(r"D:\…\Pi"),
            "the path middle-ellipsizes: {rendered}"
        );
assert!(
            rendered.contains("↑11k  ↓4.8k  2.2%/500k"),
            "arrows and ctx/window survive: {rendered}"
        );
        assert!(
            !rendered.contains("R14k") && !rendered.contains("C51"),
            "C drops before R, both before ↑↓: {rendered}"
        );
    }

    #[test]
    fn responsive_40_80_120_snapshots_keep_fixed_slots() {
        let mut app = app();
        for ch in "中文 e\u{301} 👩‍💻 and a long draft that wraps inside the composer".chars()
        {
            app.on_action(Action::InsertChar(ch));
        }
        app.on_operation_event(&FrontendOperationEvent::ToolBatchRequested {
            tool_batch_id: "batch".to_owned(),
            call_count: 1,
        });
        app.on_operation_event(&FrontendOperationEvent::ToolExecutionStarted {
            tool_batch_id: "batch".to_owned(),
            tool_call_id: "call".to_owned(),
            index: 0,
            tool_name: "read_file".to_owned(),
            arguments: "{\"path\":\"src/中文.rs\"}".to_owned(),
        });
        app.set_busy(true);
        freeze_turn(&mut app, 57);
        let snapshot = [MIN_SUPPORTED_WIDTH, 80, 120]
            .into_iter()
            .map(|width| format!("{width} columns\n{}", render(&app, width, VIEWPORT_HEIGHT)))
            .collect::<Vec<_>>()
            .join("\n\n");
        crate::tests::assert_tui_snapshot!("m14_responsive_layout", snapshot);
    }

    #[test]
    fn minimum_height_and_confirmation_keep_composer_visible() {
        let mut app = app();
        app.on_paste("draft 中文");
        app.sync_confirmation(Some((
            7,
            "run command".to_owned(),
            "cargo test -p philo-tui".to_owned(),
        )));
        let rendered = render(&app, 40, MIN_SUPPORTED_HEIGHT);
        assert!(
            rendered.contains("Approval required"),
            "the approval title survives short screens: {rendered}"
        );
        assert!(
            rendered.contains('❯'),
            "the bare prompt survives short screens: {rendered}"
        );
        assert!(rendered.contains("draft"));
    }

    #[test]
    fn history_viewport_follows_then_pages_at_24_rows() {
        let mut app = app();
        app.cells.push_closed((0..25).map(|i| TranscriptLine {
            kind: LineKind::Meta,
            text: format!("row-{i}"),
            tone: crate::app::transcript::Tone::Plain,
            header: None,
            body: None,
        }));

        let follow = render(&app, 80, 24);
        assert!(shows(&follow, "row-24"), "follow shows the tail: {follow}");
        assert!(!shows(&follow, "row-0"), "follow hides the head: {follow}");
        crate::tests::assert_tui_snapshot!("m17_history_viewport_24", follow);

        app.on_action(Action::PageTranscriptUp);
        let paged = render(&app, 80, 24);
        assert!(
            shows(&paged, "row-0"),
            "page-up reveals older rows: {paged}"
        );
        assert!(!shows(&paged, "row-24"), "page-up leaves the tail: {paged}");

        app.on_operation_event(&FrontendOperationEvent::TextDelta {
            delta: "partial answer".to_owned(),
        });
        app.flush_stream();
        let pinned = render(&app, 80, 24);
        assert!(
            shows(&pinned, "row-0"),
            "open output must not yank a paged-up view: {pinned}"
        );
        assert!(
            !shows(&pinned, "partial answer"),
            "pinned view stays on older rows: {pinned}"
        );
        assert_eq!(app.cells.cells().len(), 26, "partial is the open last cell");

        app.on_action(Action::PageTranscriptDown);
        let followed = render(&app, 80, 24);
        assert!(
            shows(&followed, "partial answer"),
            "follow-bottom shows the open tail: {followed}"
        );
    }

    #[test]
    fn transcript_selection_highlights_history_rows() {
        let mut app = app();
        app.cells.push_closed((0..30).map(|i| TranscriptLine {
            kind: LineKind::Meta,
            text: format!("row-{i}"),
            tone: crate::app::transcript::Tone::Plain,
            header: None,
            body: None,
        }));
        let _ = render(&app, 80, 24);
        let areas = panel_areas(Rect::new(0, 0, 80, 24), 0);
        let y = areas.live.y;
        let x = CONTENT_INSET;
        app.on_action(Action::SelectStart { x, y });
        app.on_action(Action::SelectDrag { x: x + 5, y });
        app.on_action(Action::SelectEnd { x: x + 5, y });

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &app, false))
            .expect("draw");
        let reversed = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|cell| cell.modifier.contains(ratatui::style::Modifier::REVERSED));
        assert!(reversed, "selected columns must paint a reversed highlight");
        assert!(app.has_selection());
        assert!(!app.follow_bottom());
    }

    #[test]
    fn inset_h_keeps_at_least_one_column() {
        let thin = inset_h(Rect::new(0, 0, 3, 1));
        assert_eq!(thin.x, 1);
        assert_eq!(thin.width, 1);
        let wide = inset_h(Rect::new(0, 0, 80, 10));
        assert_eq!(wide.x, CONTENT_INSET);
        assert_eq!(wide.width, 80 - CONTENT_INSET * 2);
    }

    /// v4.0 user message: green ❯ prefix, bold body, hanging wrap; the
    /// answer stays bare in the shared column while the box hosts ❯ too.
    #[test]
    fn user_message_wears_the_prompt_prefix_and_answers_stay_bare() {
        let mut app = app();
        app.cells.push_closed([TranscriptLine {
            kind: LineKind::User,
            text: "hello world this wraps somewhere here".to_owned(),
            tone: crate::app::transcript::Tone::Plain,
            header: None,
            body: None,
        }, TranscriptLine {
            kind: LineKind::Answer,
            text: "hello".to_owned(),
            tone: crate::app::transcript::Tone::Plain,
            header: None,
            body: None,
        }]);
        let rendered = render(&app, 40, VIEWPORT_HEIGHT);
        let user_first = rendered
            .lines()
            .find(|line| line.contains("hello world"))
            .expect("user message");
        assert!(
            user_first.contains("❯ hello world"),
            "the glyph leads the text: {user_first:?}"
        );
// Continuation lines hang past the two-cell prefix.
        let continuation = rendered
            .lines()
            .filter(|line| !line.contains("❯"))
            .find(|line| !line.trim_start().is_empty() && line.starts_with("  "))
            .expect("wrapped continuation hangs past the prefix");

        assert!(
            continuation.contains("here"),
            "the wrapped tail is the continuation: {continuation:?}"
        );
        crate::tests::assert_tui_snapshot!("m16_user_prompt_prefix", rendered);
    }
}
