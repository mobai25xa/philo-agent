//! Pure projection of app state into the isolated terminal screen.

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::app::activity::ActivityTone;
use crate::app::select::BandLayout;
use crate::app::state::App;
use crate::app::text;

use super::composer;
use super::history;
use super::inset_h;
use super::markdown::MarkdownRenderer;
use super::theme;

#[cfg(test)]
pub(crate) const VIEWPORT_HEIGHT: u16 = 12;
/// M14's reviewed responsive matrix starts here. Smaller terminals degrade
/// without panicking, but are not part of the supported layout guarantee.
#[cfg(test)]
pub(crate) const MIN_SUPPORTED_WIDTH: u16 = 40;
pub(crate) const MIN_SUPPORTED_HEIGHT: u16 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PanelAreas {
    activity: Rect,
    live: Rect,
    popover: Rect,
    composer: Rect,
    status: Rect,
}

pub(crate) fn draw(
    frame: &mut ratatui::Frame<'_>,
    app: &App,
    markdown: &MarkdownRenderer,
    _shift_enter: bool,
) {
    let areas = panel_areas(frame.area());
    draw_activity(frame, app, areas.activity);
    draw_band(frame, app, markdown, union(areas.live, areas.popover));
    draw_composer(frame, app, areas.composer);
    draw_status(frame, app, areas.status);
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

/// Fullscreen slot contract (bottom chrome):
///
/// ```text
/// fullscreen
///   transcript+band  = leftover
///   activity         = 0|1   immediately above composer, left-aligned
///   composer         = 3     borderless band immediately above status
///   status           = 0|1
/// ```
///
/// Activity is bottom chrome, not a top bar. Composer stays immediately
/// above status. History, the live stream, and the tool row share the
/// leftover band; they never move the composer.
fn panel_areas(area: Rect) -> PanelAreas {
    let height = area.height;
    let status_height = u16::from(height >= 2);
    let activity_height = u16::from(height >= 6);
    let composer_height = if height >= MIN_SUPPORTED_HEIGHT {
        3
    } else {
        height
            .saturating_sub(status_height + activity_height)
            .min(3)
    };
    let composer_height = composer_height.min(height.saturating_sub(status_height));
    let live_height = height.saturating_sub(status_height + activity_height + composer_height);

    let mut y = area.y;
    let mut take = |slot_height| {
        let slot = Rect::new(area.x, y, area.width, slot_height);
        y = y.saturating_add(slot_height);
        slot
    };
    PanelAreas {
        live: take(live_height),
        popover: take(0),
        activity: take(activity_height),
        composer: take(composer_height),
        status: take(status_height),
    }
}

fn draw_activity(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
    let area = inset_h(area);
    let Some(activity) = app.activity_view(usize::from(area.width)) else {
        return;
    };
    let style = match activity.tone {
        ActivityTone::Normal => theme::activity_normal(),
        ActivityTone::Reasoning => theme::activity_reasoning(),
        ActivityTone::Tool => theme::activity_tool(),
        ActivityTone::Warning => theme::activity_warning(),
    };
    frame.render_widget(Paragraph::new(Line::styled(activity.text, style)), area);
}

fn draw_band(frame: &mut ratatui::Frame<'_>, app: &App, markdown: &MarkdownRenderer, area: Rect) {
    let width = usize::from(area.width);
    if area.is_empty() {
        app.note_history_layout(width, 0);
        return;
    }
    if let Some(overlay) = app.overlay_frame_for(area.height.saturating_sub(2).into(), width) {
        draw_overlay(frame, area, overlay);
        app.note_history_layout(width, 0);
        return;
    }

    let hint_width = usize::from(inset_h(Rect::new(area.x, area.y, area.width, 1)).width);
    let hint = app
        .attachments()
        .summary()
        .map(|line| text::truncate(&line, hint_width));
    let hint_height = u16::from(hint.is_some());

    let menu_capacity =
        usize::from(area.height.saturating_sub(hint_height)).min(COMMAND_MENU_MAX_ROWS);
    let menu = app.command_menu_frame(hint_width, menu_capacity);
    let menu_height =
        u16::try_from(menu.as_ref().map_or(0, |frame| frame.rows.len())).unwrap_or(u16::MAX);

    let remaining = area
        .height
        .saturating_sub(hint_height)
        .saturating_sub(menu_height);
    let remaining_area = Rect::new(area.x, area.y, area.width, remaining);
    let menu_area = Rect::new(area.x, remaining_area.bottom(), area.width, menu_height);
    let hint_area = inset_h(Rect::new(
        area.x,
        menu_area.bottom(),
        area.width,
        hint_height,
    ));

    if !remaining_area.is_empty() {
        draw_remaining_band(frame, app, markdown, remaining_area);
    } else {
        app.note_history_layout(width, 0);
    }
    draw_command_menu(frame, menu, menu_area);
    if let Some(hint) = hint {
        frame.render_widget(Paragraph::new(Line::styled(hint, theme::meta())), hint_area);
    }
}

/// Upper bound on visible command-menu rows; the list scrolls inside it.
const COMMAND_MENU_MAX_ROWS: usize = 10;

fn draw_command_menu(
    frame: &mut ratatui::Frame<'_>,
    menu: Option<crate::app::state::CommandMenuFrame>,
    area: Rect,
) {
    let Some(menu) = menu else {
        return;
    };
    if area.is_empty() {
        return;
    }
    frame.render_widget(Block::default().style(theme::menu_panel()), area);
    let width = usize::from(area.width);
    for (index, row) in menu.rows.iter().enumerate() {
        let row_area = Rect::new(area.x, area.y + index as u16, area.width, 1);
        if index == menu.selected {
            let style = theme::menu_selected_row();
            let mut spans = vec![
                Span::styled(row.usage.to_owned(), style),
                Span::styled(row.summary.to_owned(), style),
            ];
            let used = text::width(&row.usage) + text::width(&row.summary);
            if used < width {
                spans.push(Span::styled(" ".repeat(width - used), style));
            }
            frame.render_widget(Paragraph::new(Line::from(spans)), row_area);
        } else {
            let line = Line::from(vec![
                Span::styled(row.usage.to_owned(), theme::menu_usage()),
                Span::styled(row.summary.to_owned(), theme::meta()),
            ]);
            frame.render_widget(Paragraph::new(line), row_area);
        }
    }
}

/// Split the leftover band (above activity/composer, above hint):
///
/// 1. If a tool timeline row exists: 1 row at the bottom; the rest is
///    history, or idle chrome when the display list is empty.
/// 2. Else if display cells are empty: idle chrome / activity details.
/// 3. Else: history gets all remaining. Do not draw the idle rule over
///    history. In-progress answer/think are the open last cell in that list.
fn draw_remaining_band(
    frame: &mut ratatui::Frame<'_>,
    app: &App,
    markdown: &MarkdownRenderer,
    area: Rect,
) {
    let column = inset_h(area);
    let width = usize::from(column.width);
    let remaining = column.height;
    let tool = app.activity_timeline_row(width);
    let cells_empty = app.cells.is_empty();

    let tool_h = u16::from(tool.is_some() && remaining > 0);
    let leftover = remaining.saturating_sub(tool_h);
    let (history_h, chrome_h) = if cells_empty {
        (0, leftover)
    } else {
        (leftover, 0)
    };

    let history_area = Rect::new(column.x, column.y, column.width, history_h);
    let chrome_area = Rect::new(column.x, history_area.bottom(), column.width, chrome_h);
    let tool_area = Rect::new(column.x, chrome_area.bottom(), column.width, tool_h);

    app.note_transcript_layout(BandLayout::from_parts(
        history_area.x,
        history_area.y,
        history_area.width,
        history_area.height,
    ));

    let selection = app.clamped_selection();

    if !history_area.is_empty() {
        let slice = app.history_slice(width, usize::from(history_h));
        frame.render_widget(
            Paragraph::new(history::paint_slice(markdown, &slice, selection, width)),
            history_area,
        );
    }
    if !chrome_area.is_empty() {
        let details = app.activity_detail_rows(width, usize::from(chrome_h));
        if !details.is_empty() {
            let lines = details
                .into_iter()
                .map(|row| Line::styled(row, theme::meta()))
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines), chrome_area);
        }
    }
    if let Some(tool) = tool
        && !tool_area.is_empty()
    {
        frame.render_widget(
            Paragraph::new(Line::styled(
                text::truncate(&tool, width),
                theme::activity_tool(),
            )),
            tool_area,
        );
    }
}

fn draw_overlay(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    overlay: crate::app::overlay::OverlayFrame,
) {
    let title_style = if overlay.title.starts_with("approval") {
        theme::activity_warning().add_modifier(Modifier::BOLD)
    } else {
        theme::activity_normal().add_modifier(Modifier::BOLD)
    };
    let mut lines = vec![Line::styled(overlay.title, title_style)];
    if area.height > 2 {
        lines.extend(overlay.body.into_iter().map(Line::from));
    }
    if area.height > 1 {
        lines.push(Line::styled(overlay.footer, theme::meta()));
    }
    lines.truncate(usize::from(area.height));
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_composer(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
    frame.render_widget(Block::default().style(theme::user_band()), area);

    let column = inset_h(area);
    let width = usize::from(column.width);
    let height = usize::from(area.height);
    let view = composer::viewport(&app.input, width, height);
    let placeholder = app.input.is_empty().then_some("Ask anything");
    let top_pad = if view.first_visual_row == 0 && view.rows.len() < height {
        1.min(height.saturating_sub(view.rows.len().max(1)))
    } else {
        0
    };
    let text_area = Rect::new(
        column.x,
        area.y.saturating_add(u16::try_from(top_pad).unwrap_or(0)),
        column.width,
        area.height
            .saturating_sub(u16::try_from(top_pad).unwrap_or(0)),
    );
    if !text_area.is_empty() {
        frame.render_widget(
            Paragraph::new(composer::styled_rows(&view.rows, placeholder))
                .style(theme::user_band()),
            text_area,
        );
    }
    if app.input_focused() {
        let cursor_x = column
            .x
            .saturating_add(u16::try_from(view.cursor_x).unwrap_or(0));
        let cursor_y = area
            .y
            .saturating_add(u16::try_from(top_pad + view.cursor_y).unwrap_or(0));
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

#[cfg(test)]
pub(crate) fn composer_y(height: u16) -> u16 {
    panel_areas(Rect::new(0, 0, 80, height)).composer.y
}

fn draw_status(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
    let raw = app.status.line_for_width(usize::from(area.width));
    let lead = if app.status.compacting || app.status.busy {
        theme::status_busy()
    } else {
        theme::status_idle()
    };
    let spans = raw
        .split(" · ")
        .enumerate()
        .flat_map(|(index, field)| {
            let style = if index == 0 {
                lead
            } else if field.starts_with("compact") || field.starts_with("queued") {
                theme::status_busy()
            } else {
                theme::meta()
            };
            let mut parts = Vec::new();
            if index > 0 {
                parts.push(Span::styled(" · ", theme::rule()));
            }
            parts.push(Span::styled(field.to_owned(), style));
            parts
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use philo_agent_service::FrontendOperationEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::app::action::Action;
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
        let markdown = MarkdownRenderer::new();
        terminal
            .draw(|frame| draw(frame, app, &markdown, false))
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
        assert_eq!(baseline.composer.height, 3);
        assert_eq!(baseline.live.height, 7);
        assert_eq!(baseline.activity.bottom(), baseline.composer.y);
        assert_eq!(baseline.live.bottom(), baseline.activity.y);
        for height in [MIN_SUPPORTED_HEIGHT, VIEWPORT_HEIGHT, 20] {
            let areas = panel_areas(Rect::new(0, 0, 80, height));
            assert_eq!(areas.activity.bottom(), areas.composer.y);
            assert_eq!(areas.composer.bottom(), areas.status.y);
        }
    }

    #[test]
    fn composer_baseline_does_not_move_for_activity_or_popovers() {
        let composer_row = |_app: &App| usize::from(composer_y(VIEWPORT_HEIGHT));
        let expected = composer_row(&app());

        let mut activity = app();
        activity.set_busy(true, 0);
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
        picker.open_picker(vec!["one".to_owned(), "two".to_owned()]);
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
            shows(&rendered, "> /sessions  pick a session to continue"),
            "the highlighted row leads the menu: {rendered}"
        );
        crate::tests::assert_tui_snapshot!("m18_command_menu", rendered);
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
        app.set_busy(true, 0);
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
        assert!(rendered.contains("Approval required"));
        assert!(rendered.contains('›'));
        assert!(rendered.contains("draft"));
    }

    #[test]
    fn history_viewport_follows_then_pages_at_24_rows() {
        let mut app = app();
        app.cells.push_closed((0..30).map(|i| TranscriptLine {
            kind: LineKind::Meta,
            text: format!("row-{i}"),
        }));

        let follow = render(&app, 80, 24);
        assert!(shows(&follow, "row-29"), "follow shows the tail: {follow}");
        assert!(!shows(&follow, "row-0"), "follow hides the head: {follow}");
        crate::tests::assert_tui_snapshot!("m17_history_viewport_24", follow);

        app.on_action(Action::PageTranscriptUp);
        let paged = render(&app, 80, 24);
        assert!(
            shows(&paged, "row-0"),
            "page-up reveals older rows: {paged}"
        );
        assert!(!shows(&paged, "row-29"), "page-up leaves the tail: {paged}");

        let areas = panel_areas(Rect::new(0, 0, 80, 24));
        assert_eq!(areas.composer.height, 3);
        assert_eq!(areas.composer.bottom(), areas.status.y);

        app.on_operation_event(&FrontendOperationEvent::TextDelta {
            delta: "partial answer".to_owned(),
        });
        let pinned = render(&app, 80, 24);
        assert!(
            shows(&pinned, "row-0"),
            "open output must not yank a paged-up view: {pinned}"
        );
        assert!(
            !shows(&pinned, "partial answer"),
            "pinned view stays on older rows: {pinned}"
        );
        assert_eq!(app.cells.cells().len(), 31, "partial is the open last cell");
        assert_eq!(app.cells.open_index(), Some(30));
        assert_eq!(
            &app.cells.cells()[30],
            &TranscriptLine {
                kind: LineKind::Answer,
                text: "partial answer".to_owned(),
            }
        );

        app.on_action(Action::PageTranscriptDown);
        let followed = render(&app, 80, 24);
        assert!(
            shows(&followed, "• partial answer"),
            "follow-bottom shows the open tail: {followed}"
        );
        assert!(
            !shows(&followed, "answer"),
            "no live-band answer header: {followed}"
        );
    }

    #[test]
    fn transcript_selection_highlights_history_rows() {
        let mut app = app();
        app.cells.push_closed((0..30).map(|i| TranscriptLine {
            kind: LineKind::Meta,
            text: format!("row-{i}"),
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
        let markdown = MarkdownRenderer::new();
        terminal
            .draw(|frame| draw(frame, &app, &markdown, false))
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
        }]);
        let rendered = render(&app, 80, VIEWPORT_HEIGHT);
        let answer = rendered
            .lines()
            .find(|line| line.contains("hello"))
            .expect("answer");
        assert!(
            answer.starts_with("    • hello"),
            "answer sits in the content column: {answer:?}"
        );
        let prompt = rendered
            .lines()
            .find(|line| line.contains("Ask anything"))
            .expect("composer");
        assert!(
            prompt.starts_with("    ›"),
            "composer text shares the column: {prompt:?}"
        );
    }
}
