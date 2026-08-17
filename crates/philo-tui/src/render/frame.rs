//! Pure projection of app state into a stable inline bottom panel.

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::activity::ActivityTone;
use crate::app::state::App;
use crate::app::text;
use crate::app::transcript::{LineKind, TranscriptLine};

use super::composer;
use super::markdown::MarkdownRenderer;
use super::theme;

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

fn panel_areas(area: Rect) -> PanelAreas {
    let height = area.height;
    let status_height = u16::from(height >= 2);
    let activity_height = u16::from(height >= 6);
    let live_height = u16::from(height >= 7);
    let minimum_popover = u16::from(height >= MIN_SUPPORTED_HEIGHT);
    let reserved = status_height + activity_height + live_height + minimum_popover;
    let composer_height = height.saturating_sub(reserved).min(6);
    let popover_height =
        height.saturating_sub(status_height + activity_height + live_height + composer_height);

    let mut y = area.y;
    let mut take = |slot_height| {
        let slot = Rect::new(area.x, y, area.width, slot_height);
        y = y.saturating_add(slot_height);
        slot
    };
    PanelAreas {
        activity: take(activity_height),
        live: take(live_height),
        popover: take(popover_height),
        composer: take(composer_height),
        status: take(status_height),
    }
}

fn draw_activity(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
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
    if area.is_empty() {
        return;
    }
    let width = usize::from(area.width);
    if let Some(overlay) = app.overlay_frame_for(area.height.saturating_sub(2).into(), width) {
        draw_overlay(frame, area, overlay);
        return;
    }

    let hint = app
        .completion_line()
        .or_else(|| app.attachments().summary())
        .map(|line| text::truncate(&line, width));
    let hint_height = u16::from(hint.is_some());
    let live_height = area.height.saturating_sub(hint_height);
    let live_area = Rect::new(area.x, area.y, area.width, live_height);
    let hint_area = Rect::new(area.x, live_area.bottom(), area.width, hint_height);

    if !live_area.is_empty() {
        if let Some((kind, partial)) = app.transcript.partial() {
            draw_live_stream(frame, markdown, live_area, kind, partial);
        } else {
            let details = app.activity_detail_rows(width, usize::from(live_height));
            if details.is_empty() {
                draw_idle_chrome(frame, app, live_area);
            } else {
                let lines = details
                    .into_iter()
                    .map(|row| Line::styled(row, theme::meta()))
                    .collect::<Vec<_>>();
                frame.render_widget(Paragraph::new(lines), live_area);
            }
        }
    }
    if let Some(hint) = hint {
        frame.render_widget(Paragraph::new(Line::styled(hint, theme::meta())), hint_area);
    }
}

fn draw_live_stream(
    frame: &mut ratatui::Frame<'_>,
    markdown: &MarkdownRenderer,
    area: Rect,
    kind: LineKind,
    partial: &str,
) {
    let width = usize::from(area.width);
    let height = usize::from(area.height);
    if height == 1 {
        let preview = TranscriptLine {
            kind,
            text: text::tail(partial, width),
        };
        frame.render_widget(Paragraph::new(live_row(markdown, kind, preview)), area);
        return;
    }
    let header = match kind {
        LineKind::Reasoning => ("think", theme::reasoning()),
        LineKind::Answer => ("answer", theme::activity_normal()),
        other => (other_label(other), theme::meta()),
    };
    let mut lines = vec![Line::styled(
        header.0.to_owned(),
        header.1.add_modifier(Modifier::BOLD),
    )];
    for row in text::tail_rows(partial, width, height.saturating_sub(1)) {
        lines.push(live_row(markdown, kind, TranscriptLine { kind, text: row }));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn live_row(markdown: &MarkdownRenderer, kind: LineKind, preview: TranscriptLine) -> Line<'static> {
    match kind {
        LineKind::Answer => markdown.preview(&preview),
        LineKind::Reasoning => Line::styled(preview.text, theme::reasoning()),
        _ => markdown.preview(&preview),
    }
}

fn other_label(kind: LineKind) -> &'static str {
    match kind {
        LineKind::User => "you",
        LineKind::Tool => "tool",
        LineKind::Notice => "note",
        LineKind::Error => "error",
        LineKind::Meta => "meta",
        LineKind::Answer => "answer",
        LineKind::Reasoning => "think",
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

fn draw_idle_chrome(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    if app.status.busy || app.activity_view(1).is_some() {
        return;
    }
    let width = usize::from(area.width);
    let mut lines = Vec::new();
    if area.height >= 1 {
        lines.push(Line::styled("─".repeat(width), theme::rule()));
    }
    if area.height >= 2 {
        lines.push(Line::from(vec![Span::styled(
            text::truncate(&app.status.model, width),
            theme::activity_normal(),
        )]));
    }
    if area.height >= 3 {
        lines.push(Line::styled(
            text::truncate(&format!("session  {}", app.status.session), width),
            theme::meta(),
        ));
    }
    if area.height >= 4 {
        lines.push(Line::styled(
            text::truncate("enter send  ·  ctrl+j newline  ·  /help", width),
            theme::meta(),
        ));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_composer(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
    let border = if app.has_overlay() {
        theme::border_warning()
    } else if app.input_focused() {
        theme::border_focus()
    } else {
        theme::border()
    };
    let title = composer_title(app, usize::from(area.width));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    let view = composer::viewport(
        &app.input,
        usize::from(inner.width),
        usize::from(inner.height),
    );
    let placeholder = app.input.is_empty().then_some("type a message");
    frame.render_widget(
        Paragraph::new(composer::styled_rows(&view.rows, placeholder)),
        inner,
    );
    if app.input_focused() {
        let cursor_x = inner
            .x
            .saturating_add(u16::try_from(view.cursor_x).unwrap_or(0));
        let cursor_y = inner
            .y
            .saturating_add(u16::try_from(view.cursor_y).unwrap_or(0));
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn composer_title(app: &App, width: usize) -> String {
    let hint = if app.has_overlay() {
        "draft kept"
    } else if app.status.busy {
        "enter queues"
    } else if !app.attachments().is_empty() {
        "enter send  ·  image attached"
    } else {
        "enter send"
    };
    let title = format!(" Message · {hint} ");
    if text::width(&title) + 2 <= width {
        title
    } else {
        " Message ".to_owned()
    }
}

fn draw_status(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
    let raw = app.status.line_for_width(usize::from(area.width));
    let spans = raw
        .split(" · ")
        .enumerate()
        .flat_map(|(index, field)| {
            let style = if field == "idle" {
                theme::status_idle()
            } else if field.starts_with("busy") || field.starts_with("compacting") {
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
    use philo_agent_runtime::{AgentEvent, ToolBatchId, ToolCallId};
    use philo_session::SessionId;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::app::action::Action;
    use crate::app::status::StatusData;
    use crate::app::transcript::InfoLevel;

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

    #[test]
    fn composer_geometry_is_independent_of_transient_state() {
        let baseline = panel_areas(Rect::new(0, 0, 80, VIEWPORT_HEIGHT));
        assert_eq!(baseline.composer.height, 6);
        assert_eq!(baseline.popover.height, 3);
        for height in [MIN_SUPPORTED_HEIGHT, VIEWPORT_HEIGHT, 20] {
            let areas = panel_areas(Rect::new(0, 0, 80, height));
            assert_eq!(areas.composer.bottom(), areas.status.y);
        }
    }

    #[test]
    fn composer_baseline_does_not_move_for_activity_or_popovers() {
        let composer_row = |app: &App| {
            render(app, 80, VIEWPORT_HEIGHT)
                .lines()
                .position(|line| line.contains("Message"))
                .expect("composer border")
        };
        let expected = composer_row(&app());

        let mut activity = app();
        activity.set_busy(true, 0);
        activity.on_agent_event(&AgentEvent::ToolBatchRequested {
            tool_batch_id: ToolBatchId::new("batch"),
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
        picker.open_picker(vec![SessionId::new("one"), SessionId::new("two")]);
        assert_eq!(composer_row(&picker), expected);

        let mut approval = app();
        approval.sync_confirmation(Some((
            crate::api::confirmation::ConfirmationId::for_test(1),
            crate::api::ConfirmationRequest {
                title: "write file".to_owned(),
                body: "src/main.rs".to_owned(),
            },
        )));
        assert_eq!(composer_row(&approval), expected);
    }

    #[test]
    fn responsive_40_80_120_snapshots_keep_fixed_slots() {
        let mut app = app();
        for ch in "中文 e\u{301} 👩‍💻 and a long draft that wraps inside the composer".chars()
        {
            app.on_action(Action::InsertChar(ch));
        }
        app.on_agent_event(&AgentEvent::ToolBatchRequested {
            tool_batch_id: ToolBatchId::new("batch"),
            call_count: 1,
        });
        app.on_agent_event(&AgentEvent::ToolExecutionStarted {
            tool_batch_id: ToolBatchId::new("batch"),
            tool_call_id: ToolCallId::new("call"),
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
            crate::api::confirmation::ConfirmationId::for_test(7),
            crate::api::ConfirmationRequest {
                title: "run command".to_owned(),
                body: "cargo test -p philo-tui".to_owned(),
            },
        )));
        let rendered = render(&app, 40, MIN_SUPPORTED_HEIGHT);
        assert!(rendered.contains("Approval required"));
        assert!(rendered.contains("Message"));
        assert!(rendered.contains("draft"));
    }
}
