//! Append-only writes to the terminal's natural scrollback.

use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::{Terminal, backend::Backend};

pub(crate) fn append_history<B: Backend>(
    terminal: &mut Terminal<B>,
    rendered: &[Line<'static>],
) -> Result<(), B::Error> {
    if rendered.is_empty() {
        return Ok(());
    }
    let height = u16::try_from(rendered.len()).unwrap_or(u16::MAX);
    terminal.insert_before(height, |buf| {
        use ratatui::widgets::Widget;
        Paragraph::new(rendered.to_vec()).render(buf.area, buf);
    })?;
    Ok(())
}
