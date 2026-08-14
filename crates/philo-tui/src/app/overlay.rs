//! Overlay state and its pure frame projection.
//!
//! Two overlays exist in v1: the session picker (`/sessions`) and the
//! approval prompt fed by the confirmation channel. Both project to an
//! [`OverlayFrame`] of plain text so the content is snapshot-testable and
//! the terminal shell only has to paint it. Overlays are transient bottom
//! panel content: they never touch the scrollback and never intercept
//! agent events.

use std::collections::HashMap;

use philo_session::SessionId;

use crate::api::confirmation::{ConfirmationId, ConfirmationRequest};

use super::text;

/// Width of the session column in the picker.
const LIST_WIDTH: usize = 22;

/// Rendered overlay content: a title, body rows, and one footer of hints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayFrame {
    pub title: String,
    pub body: Vec<String>,
    pub footer: String,
}

impl OverlayFrame {
    /// Flat text rendering (snapshot form).
    #[cfg(test)]
    pub fn to_text(&self) -> String {
        let mut text = String::from(&self.title);
        for row in &self.body {
            text.push('\n');
            text.push_str(row.trim_end());
        }
        text.push('\n');
        text.push_str(&self.footer);
        text
    }
}

/// Preview state of one session in the picker (loaded lazily, once).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Preview {
    Loading,
    Ready(Vec<String>),
    Failed(String),
}

/// The `/sessions` overlay: a list, a lazily loaded preview of the
/// highlighted session, and a selection cursor.
#[derive(Clone, Debug)]
pub struct SessionPicker {
    sessions: Vec<SessionId>,
    selected: usize,
    previews: HashMap<String, Preview>,
}

impl SessionPicker {
    /// Opens a picker over a non-empty session list.
    pub(crate) fn new(sessions: Vec<SessionId>) -> Self {
        debug_assert!(
            !sessions.is_empty(),
            "the picker needs at least one session"
        );
        Self {
            sessions,
            selected: 0,
            previews: HashMap::new(),
        }
    }

    /// The highlighted session.
    pub fn selected(&self) -> &SessionId {
        &self.sessions[self.selected]
    }

    /// Moves the highlight; returns whether it actually moved.
    pub(crate) fn move_up(&mut self) -> bool {
        if self.selected == 0 {
            return false;
        }
        self.selected -= 1;
        true
    }

    /// Moves the highlight; returns whether it actually moved.
    pub(crate) fn move_down(&mut self) -> bool {
        if self.selected + 1 >= self.sessions.len() {
            return false;
        }
        self.selected += 1;
        true
    }

    /// Claims the preview load for the current selection: yields the id
    /// the first time it is needed and marks it loading, so a preview is
    /// fetched at most once per session per overlay.
    pub(crate) fn claim_preview(&mut self) -> Option<SessionId> {
        let id = self.selected().clone();
        if self.previews.contains_key(id.as_str()) {
            return None;
        }
        self.previews
            .insert(id.as_str().to_owned(), Preview::Loading);
        Some(id)
    }

    /// Records a finished preview load.
    pub(crate) fn set_preview(&mut self, id: &SessionId, preview: Preview) {
        self.previews.insert(id.as_str().to_owned(), preview);
    }

    /// Projects the overlay content for a body of `height` rows.
    #[cfg(test)]
    pub(crate) fn frame(&self, height: usize) -> OverlayFrame {
        self.frame_for(height, 80)
    }

    pub(crate) fn frame_for(&self, height: usize, width: usize) -> OverlayFrame {
        let rows = height.max(1);
        let start = if self.selected >= rows {
            self.selected + 1 - rows
        } else {
            0
        };
        let preview = self.preview_rows();
        let show_preview = width >= 32;
        let list_width = if show_preview {
            LIST_WIDTH.min((width / 2).saturating_sub(3).max(10))
        } else {
            width.saturating_sub(2).max(1)
        };
        let body = (0..rows)
            .map(|offset| {
                let index = start + offset;
                let entry = self.sessions.get(index).map_or_else(
                    || " ".repeat(list_width + 2),
                    |id| {
                        let marker = if index == self.selected { ">" } else { " " };
                        format!("{marker} {}", cell(id.as_str(), list_width))
                    },
                );
                if !show_preview {
                    return text::truncate(&entry, width);
                }
                let preview_row = preview.get(offset).map_or("", String::as_str);
                let prefix = format!("{entry} | ");
                let available = width.saturating_sub(text::width(&prefix));
                format!("{prefix}{}", text::truncate(preview_row, available))
            })
            .collect();
        OverlayFrame {
            title: text::truncate(
                &format!("sessions ({}/{})", self.selected + 1, self.sessions.len()),
                width,
            ),
            body,
            footer: text::truncate("Enter switch | Up/Down select | Esc close", width),
        }
    }

    fn preview_rows(&self) -> Vec<String> {
        match self.previews.get(self.selected().as_str()) {
            None | Some(Preview::Loading) => vec!["loading preview...".to_owned()],
            Some(Preview::Ready(lines)) => lines.clone(),
            Some(Preview::Failed(message)) => vec![format!("preview unavailable: {message}")],
        }
    }
}

/// The approval overlay: one confirmation request at a time (FIFO), with a
/// binary answer. Approval semantics live in the external decorator; this
/// only carries the question in and the answer out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmPrompt {
    pub(crate) id: ConfirmationId,
    pub(crate) request: ConfirmationRequest,
}

impl ConfirmPrompt {
    pub(crate) fn new(id: ConfirmationId, request: ConfirmationRequest) -> Self {
        Self { id, request }
    }

    /// The question's title, echoed with the decision.
    pub fn title(&self) -> &str {
        &self.request.title
    }

    /// Projects the overlay content for a body of `height` rows.
    #[cfg(test)]
    pub(crate) fn frame(&self, height: usize) -> OverlayFrame {
        self.frame_for(height, 80)
    }

    pub(crate) fn frame_for(&self, height: usize, width: usize) -> OverlayFrame {
        let body = self
            .request
            .body
            .lines()
            .take(height.max(1))
            .map(|line| text::truncate(line, width))
            .collect();
        OverlayFrame {
            title: text::truncate(&format!("approval required: {}", self.request.title), width),
            body,
            footer: text::truncate("y allow | n / Esc deny", width),
        }
    }
}

/// Truncates and pads to exactly `width` terminal cells.
fn cell(text: &str, width: usize) -> String {
    text::pad(text, width)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker() -> SessionPicker {
        SessionPicker::new(vec![
            SessionId::new("s-1"),
            SessionId::new("s-2"),
            SessionId::new("s-3"),
        ])
    }

    #[test]
    fn previews_are_claimed_once_per_session() {
        let mut picker = picker();
        assert_eq!(picker.claim_preview(), Some(SessionId::new("s-1")));
        assert_eq!(picker.claim_preview(), None, "already loading");
        picker.set_preview(
            &SessionId::new("s-1"),
            Preview::Ready(vec!["hello".to_owned()]),
        );
        assert_eq!(picker.claim_preview(), None, "already loaded");
        assert!(picker.move_down());
        assert_eq!(picker.claim_preview(), Some(SessionId::new("s-2")));
    }

    #[test]
    fn selection_clamps_at_both_ends() {
        let mut picker = picker();
        assert!(!picker.move_up(), "already at the top");
        assert!(picker.move_down());
        assert!(picker.move_down());
        assert!(!picker.move_down(), "already at the bottom");
        assert_eq!(picker.selected(), &SessionId::new("s-3"));
    }

    #[test]
    fn the_window_follows_the_selection() {
        let mut picker = picker();
        picker.move_down();
        picker.move_down();
        let frame = picker.frame(2);
        assert!(frame.body[0].starts_with("  s-2"), "{frame:?}");
        assert!(frame.body[1].starts_with("> s-3"), "{frame:?}");
    }

    #[test]
    fn session_picker_frame_snapshot() {
        let mut picker = picker();
        picker.claim_preview();
        picker.set_preview(
            &SessionId::new("s-1"),
            Preview::Ready(vec!["> count the files".to_owned(), "one file".to_owned()]),
        );
        crate::tests::assert_tui_snapshot!("session_picker_overlay", picker.frame(5).to_text());
    }

    #[test]
    fn confirm_frame_snapshot() {
        let prompt = ConfirmPrompt::new(
            ConfirmationId::for_test(1),
            ConfirmationRequest {
                title: "run_command".to_owned(),
                body: "cargo test -p philo-tui\nworking directory: /repo".to_owned(),
            },
        );
        crate::tests::assert_tui_snapshot!("confirmation_overlay", prompt.frame(5).to_text());
    }

    #[test]
    fn long_ids_truncate_inside_the_column() {
        let picker = SessionPicker::new(vec![SessionId::new(
            "session-with-a-very-long-identifier-indeed",
        )]);
        let frame = picker.frame(1);
        assert!(
            frame.body[0].starts_with("> session-with-a-very..."),
            "{frame:?}"
        );
    }

    #[test]
    fn narrow_picker_omits_preview_and_respects_cell_width() {
        let picker = SessionPicker::new(vec![SessionId::new("中文-session-name")]);
        let frame = picker.frame_for(2, 20);
        assert!(frame.body.iter().all(|line| text::width(line) <= 20));
        assert!(!frame.body[0].contains(" | "));
    }
}
