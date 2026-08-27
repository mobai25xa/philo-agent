//! Pure projection of app state into the isolated terminal screen.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::overlay::{OverlayFrame, OverlayLine};
use crate::app::select::BandLayout;
use crate::app::state::App;
use crate::app::text;

use super::composer;
use super::history;
use super::inset_band;
use super::inset_h;
use super::stream_anchor_rows;
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
    composer: Rect,
}

pub(crate) fn draw(frame: &mut ratatui::Frame<'_>, app: &App, _shift_enter: bool) {
    app.note_frame_height(frame.area().height);
    let areas = panel_areas(frame.area());
    draw_band(frame, app, union(areas.live, areas.popover));
    draw_composer(frame, app, areas.composer);
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

/// Fullscreen slot contract (redesign §2.3):
///
/// ```text
/// fullscreen
///   transcript band  = leftover
///   composer         = 5     corner row / input 3 (surface + bar,
///                            both inset to the content column) / corner row
/// ```
///
/// Two slots only: transcript and composer band. There is no header, no
/// status bar, no hints row, no Activity row. The composer baseline never
/// moves for transient state. The surface wash plus accent bar wrap only
/// the middle input box and span exactly the shared content column, so the
/// band's edges align with the corner rows flanking it on the native
/// terminal background — no blank rows in between (v2.3 compaction).
///
/// Short screens degrade deterministically (`composer_height_for`): the
/// corner rows vanish first, then the input drops to one row.
fn panel_areas(area: Rect) -> PanelAreas {
    let composer_height = composer_height_for(area.height);
    let live_height = area.height.saturating_sub(composer_height);

    let mut y = area.y;
    let mut take = |slot_height| {
        let slot = Rect::new(area.x, y, area.width, slot_height);
        y = y.saturating_add(slot_height);
        slot
    };
    PanelAreas {
        popover: take(0),
        live: take(live_height),
        composer: take(composer_height),
    }
}

/// Composer band height ladder: the full nine-row dashboard once there is
/// room for it plus a minimal live band (the same three-row floor the old
/// supported split guaranteed), then the bare input box, then one line.
fn composer_height_for(height: u16) -> u16 {
    if height == 0 {
        return 0;
    }
    if height >= theme::COMPOSER_ROWS + 3 {
        return theme::COMPOSER_ROWS;
    }
    if height >= theme::INPUT_ROWS {
        return theme::INPUT_ROWS;
    }
    1
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

    // The menu spans exactly the input band's width and anchors at its
    // left edge, so it sits flush over the composer box (v0.44 §4.1).
    let band_column = inset_band(Rect::new(area.x, area.y, area.width, 1));
    // The rounded panel spends one row on each border.
    let menu_capacity = usize::from(area.height.saturating_sub(2)).min(theme::MENU_MAX_ROWS);
    if menu_capacity == 0 {
        draw_remaining_band(frame, app, area);
        return;
    }
    let menu = app.command_menu_frame(usize::from(band_column.width), menu_capacity);
    let menu_height =
        u16::try_from(menu.as_ref().map_or(0, |menu| menu.rows.len() + 2)).unwrap_or(u16::MAX);

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

/// The command-menu float (v0.44 §4.1): a rounded panel spanning exactly
/// the input band's width, anchored at its left edge directly above the
/// composer box.
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
    let outer_h = u16::try_from(menu.rows.len() + 2)
        .unwrap_or(u16::MAX)
        .min(area.height);
    if outer_w == 0 || outer_h <= 2 {
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
    // Text sits one padding cell inside the borders.
    let zone = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );

    for (index, row) in menu.rows.iter().enumerate() {
        let y = zone.y.saturating_add(index as u16);
        if index >= usize::from(zone.height) || y >= zone.bottom() {
            break;
        }
        let full_row = Rect::new(inner.x, y, inner.width, 1);
        let row_area = Rect::new(zone.x, y, zone.width, 1);
        if index == menu.selected {
            // Full-row accent tint including the padding cells; the base
            // style spreads the tint over the whole text zone.
            frame.render_widget(Block::default().style(theme::menu_selected_row()), full_row);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(row.usage.clone(), theme::menu_selected_row()),
                    Span::styled(row.summary.clone(), theme::menu_selected_row()),
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

/// Paints a rounded float panel (`╭ ╮ ╰ ╯ ─ │`) on the native terminal
/// background (v0.44 — floats lost their surface wash; the rect is cleared
/// so transcript rows never bleed through), with an optional title embedded
/// into the top border (`╭─ Title ───╮`). Returns the inner drawing area
/// between the borders.
fn paint_panel(frame: &mut ratatui::Frame<'_>, area: Rect, title: Option<(&str, Style)>) -> Rect {
    frame.render_widget(Clear, area);
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
/// in that list. Non-empty user rows first get their full-width surface
/// band and column-0 accent bar painted underneath (design §3.1/§3.2).
///
/// v2.2 streaming anchors: while a turn is lifted the visible window grows
/// from the 40% line and pins at the 80% line, leaving the band's tail
/// rows blank; settlement animates it back to the full band. The layout
/// note records the actually drawn sub-area so selection mapping matches.
fn draw_remaining_band(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let column = inset_h(area);
    let width = usize::from(column.width);

    // Anchors are shares of the FULL screen height (recorded by `draw`),
    // clamped into this band; `None` keeps plain bottom-follow.
    let anchors = stream_anchor_rows(app.frame_height.get(), column.height);
    let view_height = app.transcript_viewport_height(column.height, anchors);
    let view = Rect {
        height: view_height.min(column.height),
        ..column
    };

    let selection = app.clamped_selection();
    let slice = app.history_slice(width, usize::from(view.height));

    // While the stream owns the viewport (lift/pin/settle), content hangs
    // from its tail: the newest row glues to the viewport floor — the 40%
    // line while lifting, the 80% line while pinned, descending during the
    // settle drop — leaving blank rows above sparse output. Plain layouts
    // (including a user-pinned scroll) stay top-aligned.
    let bottom_align = app.stream_anchor_active();
    let painted = usize::from(view.height).min(slice.total_rows);
    let paint_y = if bottom_align {
        view.bottom()
            .saturating_sub(u16::try_from(painted).unwrap_or(u16::MAX))
    } else {
        view.y
    };
    let paint = Rect {
        y: paint_y,
        height: u16::try_from(painted).unwrap_or(u16::MAX).min(view.height),
        ..view
    };

    app.note_transcript_layout(BandLayout::from_parts(
        paint.x,
        paint.y,
        paint.width,
        paint.height,
    ));
    app.note_band_height(column.height);

    if paint.is_empty() {
        return;
    }
    paint_user_strips(frame, &slice, area, paint.y);
    frame.render_widget(
        Paragraph::new(history::paint_slice(&slice, selection, width)),
        paint,
    );
}

/// Input-band surface + accent bar under every visible non-empty user row
/// (v2.3: the band matches the composer box, one column wider than the
/// content column on each side). Wrapped continuations are ordinary rows of
/// the same cell, so the bar runs through them; the blank separator rows
/// stay untouched.
fn paint_user_strips(
    frame: &mut ratatui::Frame<'_>,
    slice: &crate::app::cells::VisibleSlice,
    area: Rect,
    top_y: u16,
) {
    let band = inset_band(area);
    let bar_style = theme::accent().bg(theme::band_rgb());
    for (index, row) in slice.rows.iter().enumerate() {
        if row.kind != crate::app::transcript::LineKind::User || row.text.is_empty() {
            continue;
        }
        let y = top_y.saturating_add(u16::try_from(index).unwrap_or(0));
        if y >= area.bottom() {
            break;
        }
        frame.render_widget(
            Block::default().style(theme::surface()),
            Rect::new(band.x, y, band.width, 1),
        );
        frame.render_widget(
            Paragraph::new(Line::styled(theme::BAR.to_owned(), bar_style)),
            Rect::new(band.x, y, 1, 1),
        );
    }
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

    let title_style = match overlay.tone {
        OverlayTone::Warning => theme::warn().add_modifier(Modifier::BOLD),
        OverlayTone::Normal => theme::primary(),
    };
    let inner = paint_panel(frame, panel, Some((&overlay.title, title_style)));
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
    frame.render_widget(
        Paragraph::new(Line::styled(overlay.footer.clone(), theme::meta())),
        Rect::new(zone.x, footer_row, zone.width, 1),
    );

    for (index, line) in overlay.body.iter().enumerate() {
        let y = zone.y.saturating_add(index as u16);
        if y >= footer_row {
            break;
        }
        draw_overlay_line(frame, Rect::new(inner.x, y, inner.width, 1), line, &zone);
    }
}

/// Paints one overlay row: the text lives in the padded zone while the
/// selected-row accent tint fills the full inner width. Unselected rows sit
/// bare on the native background (v0.44).
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
            theme::meta().add_modifier(Modifier::ITALIC),
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
        } if line.selected => vec![Span::styled(
            text::truncate(&format!("› {primary}{tail}"), width),
            theme::menu_selected_row(),
        )],
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

/// The composer band (redesign §2.3, v2.3 compaction): the surface wash and
/// the accent bar wrap only the middle input box and span the input band —
/// one column wider than the shared content column on each side, so the
/// band overhangs the corner rows; the draft rides
/// [`theme::STRIP_TEXT_INSET`] cells in from the band edge. Each one-row
/// dashboard slot hugs the box directly on the native terminal background —
/// no blank rows in between.
fn draw_composer(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
    let column = inset_h(area);
    let band = inset_band(area);
    let side = if area.height >= theme::COMPOSER_ROWS {
        theme::CORNER_ROWS
    } else {
        0
    };
    let box_area = Rect::new(
        band.x,
        area.y + side,
        band.width,
        area.height.saturating_sub(side * 2),
    );

    frame.render_widget(Block::default().style(theme::surface()), box_area);
    let bar_style = theme::accent().bg(theme::band_rgb());
    let bar: Vec<Line<'static>> = (0..box_area.height)
        .map(|_| Line::styled(theme::BAR.to_owned(), bar_style))
        .collect();
    frame.render_widget(
        Paragraph::new(bar),
        Rect {
            width: 1,
            ..box_area
        },
    );

    // The draft rides the strip text inset inside the band, keeping one
    // cell of air after the bar; corners keep the shared content column.
    let text_pad = theme::STRIP_TEXT_INSET.min(box_area.width.saturating_sub(1) / 2);
    let input_area = Rect::new(
        box_area.x.saturating_add(text_pad),
        box_area.y,
        box_area.width.saturating_sub(text_pad.saturating_mul(2)),
        box_area.height,
    );
    let width = usize::from(input_area.width);
    let height = usize::from(input_area.height);
    let view = composer::viewport(&app.input, width, height);
    let placeholder = app.input.is_empty().then_some("Ask anything");
    if !input_area.is_empty() {
        frame.render_widget(
            Paragraph::new(composer::styled_rows(&view, placeholder)).style(theme::surface()),
            input_area,
        );
    }
    if app.input_focused() && !input_area.is_empty() {
        let cursor_x = input_area
            .x
            .saturating_add(u16::try_from(view.cursor_x).unwrap_or(0));
        let cursor_y = input_area
            .y
            .saturating_add(u16::try_from(view.cursor_y).unwrap_or(0));
        frame.set_cursor_position((cursor_x, cursor_y));
    }
    if side > 0 {
        draw_top_corner(frame, app, area.y, column);
        draw_bottom_corner(frame, app, area.bottom().saturating_sub(1), column);
    }
}

/// Top band row (§2.4), painted on the native background above the input
/// box. The top-left carries the run-state word (`⠹ {State}… {elapsed} ·
/// esc cancel`); the top-right carries `({provider}) {model} · {effort}`
/// with dim chrome around a primary model name. On narrow screens the left
/// corner degrades first (§3.10).
fn draw_top_corner(frame: &mut ratatui::Frame<'_>, app: &App, row: u16, column: Rect) {
    let column_width = usize::from(column.width);
    let right = app.status.model_corner_for(column_width);
    let right_width = right.as_ref().map_or(0, model_corner_width);
    let left_budget = column_width.saturating_sub(right_width + CORNER_GAP_CELLS);

    if let Some(state) = app.run_state_corner(left_budget)
        && state.painted_width() <= left_budget
    {
        let mut spans = vec![
            Span::styled(state.spinner.clone(), theme::accent()),
            Span::raw(" "),
            Span::styled(
                state.word.clone(),
                if state.warning {
                    theme::warn()
                } else {
                    theme::primary()
                },
            ),
        ];
        if !state.timing.is_empty() {
            spans.push(Span::styled(
                format!(" {}", state.timing),
                theme::corner_meta(),
            ));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(
                column.x,
                row,
                u16::try_from(state.painted_width()).unwrap_or(1),
                1,
            ),
        );
    }

    let Some(corner) = right else {
        return;
    };
    let mut width = text::width(&corner.model);
    let mut spans = Vec::new();
    if let Some(provider) = &corner.provider {
        width += text::width(provider) + 3;
        spans.push(Span::styled(format!("({provider}) "), theme::corner_meta()));
    }
    spans.push(Span::styled(corner.model.clone(), theme::primary()));
    if let Some(effort) = &corner.effort {
        width += text::width(effort) + 3;
        spans.push(Span::styled(format!(" · {effort}"), theme::corner_meta()));
    }
    paint_right_aligned(frame, spans, width, row, column);
}

/// Cells kept clear between the two top corners when both are present.
const CORNER_GAP_CELLS: usize = 2;

/// Display-cell width of the right-top model corner.
fn model_corner_width(corner: &crate::app::status::ModelCorner) -> usize {
    let mut width = text::width(&corner.model);
    if let Some(provider) = &corner.provider {
        width += text::width(provider) + 3;
    }
    if let Some(effort) = &corner.effort {
        width += text::width(effort) + 3;
    }
    width
}

/// Bottom band row (§2.4), on the native background below the input box:
/// workspace root on the left, latest-turn usage on the right, both corner
/// chrome.
fn draw_bottom_corner(frame: &mut ratatui::Frame<'_>, app: &App, row: u16, column: Rect) {
    let (path, usage) = app.status.bottom_corners_for(usize::from(column.width));
    if !path.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(path.clone(), theme::corner_meta())),
            Rect::new(
                column.x,
                row,
                u16::try_from(text::width(&path)).unwrap_or(1),
                1,
            ),
        );
    }
    if !usage.is_empty() {
        paint_right_aligned(
            frame,
            vec![Span::styled(usage.clone(), theme::corner_meta())],
            text::width(&usage),
            row,
            column,
        );
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
    panel_areas(Rect::new(0, 0, 80, height)).composer.y
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
        rendered.lines().any(|line| line.trim() == text)
    }

    #[test]
    fn composer_geometry_is_independent_of_transient_state() {
        let baseline = panel_areas(Rect::new(0, 0, 80, VIEWPORT_HEIGHT));
        assert_eq!(baseline.composer.height, theme::COMPOSER_ROWS);
        assert_eq!(baseline.live.height, VIEWPORT_HEIGHT - theme::COMPOSER_ROWS);
        for height in [MIN_SUPPORTED_HEIGHT, VIEWPORT_HEIGHT, 20] {
            let areas = panel_areas(Rect::new(0, 0, 80, height));
            assert_eq!(areas.live.bottom(), areas.composer.y);
            assert_eq!(areas.composer.bottom(), height, "band owns the floor");
        }
    }

    #[test]
    fn short_screens_degrade_input_first_and_corners_last() {
        assert_eq!(composer_height_for(VIEWPORT_HEIGHT), theme::COMPOSER_ROWS);
        assert_eq!(
            composer_height_for(theme::COMPOSER_ROWS + 3),
            theme::COMPOSER_ROWS
        );
        assert_eq!(
            composer_height_for(theme::COMPOSER_ROWS + 3 - 1),
            theme::INPUT_ROWS,
            "corner rows vanish before the live band drops under three"
        );
        assert_eq!(
            composer_height_for(MIN_SUPPORTED_HEIGHT),
            theme::COMPOSER_ROWS
        );
        assert_eq!(composer_height_for(3), 3);
        assert_eq!(composer_height_for(2), 1);
        assert_eq!(composer_height_for(0), 0);
    }

    #[test]
    fn the_idle_screen_is_transcript_plus_band_only() {
        let rendered = render(&app(), 80, VIEWPORT_HEIGHT);
        let rows: Vec<&str> = rendered.lines().collect();
        let band_top = usize::from(composer_y(VIEWPORT_HEIGHT));

        // Idle keeps the TL corner empty and bare. The band spans the shared
        // content column; the placeholder rides the middle line, the shared
        // inset in from the bar.
        assert_eq!(
            rows[band_top],
            format!("{}gpt-test", " ".repeat(68)),
            "model corner hugs the content column's right edge"
        );
        assert_eq!(rows[band_top + 1], "   ▌", "top pad row keeps the bar");
        assert_eq!(
            rows[band_top + 2],
            format!(
                "   ▌{}Ask anything",
                " ".repeat(usize::from(theme::STRIP_TEXT_INSET - 1))
            ),
            "placeholder opens the input box: {rendered}"
        );
        assert_eq!(rows[band_top + 3], "   ▌", "below the placeholder");
        assert_eq!(
            rows[band_top + 4],
            format!("{}↑- ↓- R- C- -/-", " ".repeat(61)),
            "usage hugs the content column's right edge"
        );
        assert_eq!(rows.len() - band_top, usize::from(theme::COMPOSER_ROWS));
    }

    #[test]
    fn composer_baseline_does_not_move_for_activity_or_popovers() {
        let composer_row = |_app: &App| usize::from(composer_y(VIEWPORT_HEIGHT));
        let expected = composer_row(&app());

        let mut activity = app();
        activity.set_busy(true);
        activity.on_operation_event(&FrontendOperationEvent::ToolBatchRequested {
            tool_batch_id: "batch".to_owned(),
            call_count: 1,
        });
        assert_eq!(composer_row(&activity), expected);

        let mut completion = app();
        completion.on_paste("/s");
        completion.on_action(Action::Complete);
        assert_eq!(composer_row(&completion), expected);

        let mut attachments = app();
        attachments.attach_image("image/png".to_owned(), vec![0; 4], "clipboard");
        assert_eq!(composer_row(&attachments), expected);

        let mut picker = app();
        picker.open_picker(vec![
            PickerEntry::untitled("one"),
            PickerEntry::untitled("two"),
        ]);
        assert_eq!(composer_row(&picker), expected);

        let mut approval = app();
        approval.sync_confirmation(Some((1, "write file".to_owned(), "src/main.rs".to_owned())));
        assert_eq!(composer_row(&approval), expected);
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
                .any(|row| row.contains("› /sessions  pick a session to continue")),
            "the highlighted row leads the menu: {rendered}"
        );
        crate::tests::assert_tui_snapshot!("m18_command_menu", rendered);
    }

    /// Design §3.6: the auto menu floats as a rounded panel anchored at the
    /// content column's left edge, directly above the composer band.
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

    /// Design §3.9: the approval prompt is a centered warn-titled dialog;
    /// the composer band below keeps its draft and cursor position.
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
                .any(|row| row.trim_start().starts_with("╭─ Approval required")),
            "the title embeds into the top border: {rendered}"
        );
        assert!(
            rendered
                .lines()
                .any(|row| row.contains("y allow · n / esc deny")),
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

    #[test]
    fn the_run_state_word_lands_in_the_top_left_corner() {
        let mut app = dashboard_app();
        app.set_busy(true);
        app.on_operation_event(&FrontendOperationEvent::TextDelta {
            delta: "streaming".to_owned(),
        });
        app.flush_stream();
        freeze_turn(&mut app, 42);

        let rendered = render(&app, 80, VIEWPORT_HEIGHT);
        let band_top = usize::from(composer_y(VIEWPORT_HEIGHT));
        let rows: Vec<&str> = rendered.lines().collect();
        assert!(
            rows[band_top].starts_with("    ⠋ Writing… 42s · esc cancel"),
            "the state word opens the content column: {:?}",
            rows[band_top]
        );
        assert!(
            rows[band_top].ends_with("(openai) gpt-5.2 · high"),
            "the model corner survives beside it: {:?}",
            rows[band_top]
        );
        crate::tests::assert_tui_snapshot!("m3_writing_state", rendered);

        // Settlement empties the corner again.
        app.on_operation_event(&FrontendOperationEvent::OperationSettled {
            operation_id: "op".to_owned(),
            session_id: "s".to_owned(),
            status: "Succeeded".to_owned(),
            durability: "Confirmed".to_owned(),
            session_revision: philo_agent_service::SettlementRevision::Unchanged,
        });
        let settled_rows = render(&app, 80, VIEWPORT_HEIGHT);
        assert!(
            !settled_rows.contains("Writing…") && !settled_rows.contains("esc cancel"),
            "settlement clears the word and timer: {settled_rows}"
        );
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
        let band_top = usize::from(composer_y(VIEWPORT_HEIGHT));
        let rows: Vec<&str> = rendered.lines().collect();
        assert!(
            rows[band_top].contains("⠋ Approval… 12s · esc cancel"),
            "the overlay flag hides the underlying word: {:?}",
            rows[band_top]
        );
        crate::tests::assert_tui_snapshot!("m3_approval_overlay", rendered);

        app.sync_confirmation(None);
        let revealed = render(&app, 80, VIEWPORT_HEIGHT);
        let rows: Vec<&str> = revealed.lines().collect();
        assert!(
            rows[band_top].contains("⠋ Waiting… 12s · esc cancel"),
            "resolving reveals the underlying word: {:?}",
            rows[band_top]
        );
    }

    #[test]
    fn the_idle_dashboard_fills_three_band_corners() {
        let rendered = render(&dashboard_app(), 80, VIEWPORT_HEIGHT);
        let rows: Vec<&str> = rendered.lines().collect();
        let band_top = usize::from(composer_y(VIEWPORT_HEIGHT));

        // The idle TL corner stays empty; the model corner sits on the
        // right content edge.
        assert!(rows[band_top].ends_with("(openai) gpt-5.2 · high"));
        assert!(
            rows[band_top + 1] == "   ▌",
            "the band hugs the corner row directly, bar on the input band"
        );
        assert!(
            !rows[band_top + 2].contains("gpt-5.2"),
            "the input row carries no dashboard content"
        );
        assert!(
            rows[band_top + 4].contains(r"D:\Code\Zed\Year2026\Jul0706\Pi")
                && rows[band_top + 4].ends_with("↑11k ↓4.8k R14k C51.3% 2.2%/500k"),
            "root left, usage right on the bottom edge: {:?}",
            rows[band_top + 4]
        );

        // The model name wears the primary token; its chrome stays meta.
        crate::tests::assert_tui_snapshot!("m2_idle_dashboard", rendered);
    }

    #[test]
    fn the_bottom_row_degrades_path_first_then_c_then_r() {
        // 40 columns (design §3.10): compact path, then usage loses C% and
        // R while arrows and ctx/window survive.
        let rendered = render(&dashboard_app(), MIN_SUPPORTED_WIDTH, VIEWPORT_HEIGHT);
        assert!(
            rendered.contains(r"D:\…\Pi"),
            "the path middle-ellipsizes: {rendered}"
        );
        assert!(
            rendered.contains("↑11k ↓4.8k 2.2%/500k"),
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
            rendered.contains("▌"),
            "the band bar survives short screens"
        );
        assert!(rendered.contains("draft"));
    }

    #[test]
    fn history_viewport_follows_then_pages_at_24_rows() {
        // 25 closed rows: one page down from a page-up position reaches the
        // bottom again at the current 15-row live band.
        let mut app = app();
        app.cells.push_closed((0..25).map(|i| TranscriptLine {
            kind: LineKind::Meta,
            text: format!("row-{i}"),
            tone: crate::app::transcript::Tone::Plain,
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

        let areas = panel_areas(Rect::new(0, 0, 80, 24));
        assert_eq!(areas.composer.height, theme::COMPOSER_ROWS);
        assert_eq!(areas.composer.bottom(), 24);

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
        assert_eq!(app.cells.open_index(), Some(25));
        assert_eq!(
            &app.cells.cells()[25],
            &TranscriptLine {
                kind: LineKind::Answer,
                text: "partial answer".to_owned(),
                tone: crate::app::transcript::Tone::Plain,
            }
        );

        app.on_action(Action::PageTranscriptDown);
        let followed = render(&app, 80, 24);
        assert!(
            shows(&followed, "partial answer"),
            "follow-bottom shows the open tail: {followed}"
        );
        assert!(
            !shows(&followed, "answer header"),
            "no live-band answer header: {followed}"
        );
    }

    #[test]
    fn transcript_selection_highlights_history_rows() {
        let mut app = app();
        app.cells.push_closed((0..30).map(|i| TranscriptLine {
            kind: LineKind::Meta,
            text: format!("row-{i}"),
            tone: crate::app::transcript::Tone::Plain,
        }));
        let _ = render(&app, 80, 24);
        let areas = panel_areas(Rect::new(0, 0, 80, 24));
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

    #[test]
    fn transcript_and_composer_text_share_the_inset_column() {
        let mut app = app();
        app.cells.push_closed([TranscriptLine {
            kind: LineKind::Answer,
            text: "hello".to_owned(),
            tone: crate::app::transcript::Tone::Plain,
        }]);
        let rendered = render(&app, 80, VIEWPORT_HEIGHT);
        let answer = rendered
            .lines()
            .find(|line| line.contains("hello"))
            .expect("answer");
        assert!(
            answer.starts_with("    hello"),
            "answer sits bare in the content column: {answer:?}"
        );
        let prompt = rendered
            .lines()
            .find(|line| line.contains("Ask anything"))
            .expect("composer");
        assert!(
            prompt.starts_with(&format!(
                "{}{}",
                " ".repeat(usize::from(theme::BAND_INSET)),
                theme::BAR
            )),
            "the band bar rides the input band: {prompt:?}"
        );
        let text_column = prompt
            .find("Ask anything")
            .map(|byte| prompt[..byte].chars().count())
            .expect("placeholder text");
        assert_eq!(
            text_column,
            usize::from(theme::BAND_INSET + theme::STRIP_TEXT_INSET),
            "the draft rides the strip text inset inside the band"
        );

        crate::tests::assert_tui_snapshot!("m1_idle_skeleton", rendered);
    }
}
