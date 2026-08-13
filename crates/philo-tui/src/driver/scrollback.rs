//! Append-only writes to the terminal's natural scrollback.

use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::app::transcript::TranscriptLine;
use crate::platform::terminal::TerminalSession;
use crate::render::line::styled_line;

pub(crate) fn append_history(
    session: &mut TerminalSession,
    lines: &[TranscriptLine],
) -> std::io::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let rendered: Vec<Line<'static>> = lines.iter().map(styled_line).collect();
    let height = u16::try_from(rendered.len()).unwrap_or(u16::MAX);
    session.terminal.insert_before(height, |buf| {
        use ratatui::widgets::Widget;
        Paragraph::new(rendered).render(buf.area, buf);
    })?;
    Ok(())
}
