//! Draft editing, submit, attachments, and input-history recall.

use super::App;
use super::line;
use crate::app::attachment::PendingAttachment;
use crate::app::effect::Effect;
use crate::app::transcript::LineKind;

impl App {
    /// Pastes text verbatim (bracketed paste never submits).
    pub fn on_paste(&mut self, text: &str) -> Vec<Effect> {
        if self.confirm.is_some() || self.picker.is_some() {
            return vec![];
        }
        self.exit_armed = false;
        self.completion = None;
        self.clear_selection();
        self.history.reset_browse();
        self.bump_draft_generation();
        self.input.insert_str(text);
        self.disarm_quit_unless_typing_quit();
        vec![]
    }

    /// Queues an image the driver decoded from the clipboard.
    pub(crate) fn attach_image(
        &mut self,
        media_type: String,
        bytes: Vec<u8>,
        origin: &str,
    ) -> Vec<Effect> {
        self.bump_draft_generation();
        let attachment = PendingAttachment::Image {
            media_type,
            bytes,
            origin: origin.to_owned(),
        };
        let label = attachment.label();
        self.attachments.push(attachment);
        vec![Effect::Append(vec![line(
            LineKind::Meta,
            format!(
                "attached: {label} ({} waiting for the next message)",
                self.attachments.len()
            ),
        )])]
    }

    /// The clipboard held nothing usable: say so and point at `/image`,
    /// leaving the draft untouched.
    pub(crate) fn clipboard_unavailable(&self, reason: &str) -> Vec<Effect> {
        vec![Effect::Append(vec![line(
            LineKind::Notice,
            format!("no image on the clipboard ({reason}); attach a file with /image <path>"),
        )])]
    }

    /// Puts a refused message back for editing: the text returns to the
    /// input and the attachments that did resolve stay queued.
    pub(crate) fn restore_draft(&mut self, text: &str, attachments: Vec<PendingAttachment>) {
        self.bump_draft_generation();
        self.input.set_text(text);
        self.attachments.extend(attachments);
    }

    /// Identity captured when the driver starts resolving one submitted draft.
    pub(crate) fn draft_generation(&self) -> u64 {
        self.draft_generation
    }

    /// Restores an asynchronously refused send only if the user has not edited
    /// or submitted another draft in the meantime.
    pub(crate) fn restore_draft_if_current(
        &mut self,
        generation: u64,
        text: &str,
        attachments: Vec<PendingAttachment>,
    ) -> bool {
        if self.draft_generation != generation {
            return false;
        }
        self.restore_draft(text, attachments);
        true
    }

    pub(super) fn insert_char(&mut self, ch: char) -> Vec<Effect> {
        self.clear_selection();
        self.bump_draft_generation();
        self.history.reset_browse();
        self.input.insert_char(ch);
        self.disarm_quit_unless_typing_quit();
        vec![]
    }

    pub(super) fn insert_newline(&mut self) -> Vec<Effect> {
        self.clear_selection();
        self.bump_draft_generation();
        self.history.reset_browse();
        self.input.insert_newline();
        self.disarm_quit_unless_typing_quit();
        vec![]
    }

    pub(super) fn backspace(&mut self) -> Vec<Effect> {
        self.clear_selection();
        self.bump_draft_generation();
        self.input.backspace();
        self.disarm_quit_unless_typing_quit();
        vec![]
    }

    pub(super) fn delete(&mut self) -> Vec<Effect> {
        self.clear_selection();
        self.bump_draft_generation();
        self.input.delete();
        self.disarm_quit_unless_typing_quit();
        vec![]
    }

    pub(super) fn move_left(&mut self) -> Vec<Effect> {
        self.input.move_left();
        vec![]
    }

    pub(super) fn move_right(&mut self) -> Vec<Effect> {
        self.input.move_right();
        vec![]
    }

    pub(super) fn home(&mut self) -> Vec<Effect> {
        if self.input.at_line_start() {
            self.jump_transcript_top();
        } else {
            self.input.home();
        }
        vec![]
    }

    pub(super) fn end(&mut self) -> Vec<Effect> {
        if self.input.at_line_end() {
            self.jump_transcript_bottom();
        } else {
            self.input.end();
        }
        vec![]
    }

    pub(super) fn move_up(&mut self) -> Vec<Effect> {
        if !self.input.move_up() {
            self.history_prev();
        }
        vec![]
    }

    pub(super) fn move_down(&mut self) -> Vec<Effect> {
        if !self.input.move_down() {
            self.history_next();
        }
        vec![]
    }

    pub(super) fn submit(&mut self) -> Vec<Effect> {
        self.clear_selection();
        if self.input.is_empty() {
            return vec![];
        }
        let text = self.input.take_text();
        self.bump_draft_generation();
        self.completion = None;
        self.history.push(text.clone());

        // A `/` prefix is a command: it never reaches the model.
        if text.starts_with('/') {
            return self.run_command(&text);
        }
        self.quit_armed = false;

        let attachments = self.attachments.take();
        let mut rows: Vec<String> = text.split('\n').map(str::to_owned).collect();
        for attachment in &attachments {
            rows.push(format!("[attached {}]", attachment.label()));
        }
        let mut lines = crate::app::transcript::user_block(rows);
        if self.status.compacting {
            lines.push(line(
                LineKind::Notice,
                "compacting: the message is queued behind context maintenance",
            ));
        } else if self.status.busy {
            lines.push(line(
                LineKind::Notice,
                "busy: the message is queued behind the active turn",
            ));
        }
        vec![Effect::Append(lines), Effect::Submit { text, attachments }]
    }

    fn history_prev(&mut self) {
        if let Some(text) = self.history.prev(&self.input.text()) {
            self.bump_draft_generation();
            self.input.set_text(&text);
        }
    }

    fn history_next(&mut self) {
        if let Some(text) = self.history.next() {
            self.bump_draft_generation();
            self.input.set_text(&text);
        }
    }
}
