//! Draft editing, submit, attachments, and input-history recall.

use philo_agent_service::CommandReject;

use super::App;
use super::line;
use crate::app::attachment::PendingAttachment;
use crate::app::effect::Effect;
use crate::app::submit::{
    IntentId, PendingSubmission, SubmitDispatchResult, SubmitState,
};
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
        // At most one pending submission.
        if !matches!(self.submit_state, SubmitState::Editing | SubmitState::Accepted { .. }) {
            return vec![Effect::Append(vec![line(
                LineKind::Notice,
                "submit already in flight; wait for it to finish or fail",
            )])];
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
        let intent_id = self.allocate_intent_id();
        let held_generation = self.draft_generation;
        self.submit_state = SubmitState::Dispatching(PendingSubmission {
            intent_id,
            draft: text.clone(),
            attachments: attachments.clone(),
            request_id: None,
            held_generation,
        });

        let mut notices = Vec::new();
        if self.status.compacting {
            notices.push(line(
                LineKind::Notice,
                "compacting: the message is queued behind context maintenance",
            ));
        } else if self.status.busy {
            notices.push(line(
                LineKind::Notice,
                "busy: the message is queued behind the active turn",
            ));
        }
        let mut effects = Vec::new();
        if !notices.is_empty() {
            effects.push(Effect::Append(notices));
        }
        effects.push(Effect::PrepareSubmit {
            intent_id,
            text,
            attachments,
        });
        effects
    }

    pub(super) fn on_submit_media_refused(
        &mut self,
        intent_id: IntentId,
        kept: Vec<PendingAttachment>,
        errors: Vec<String>,
    ) -> Vec<Effect> {
        let Some(pending) = self.take_pending_if(intent_id) else {
            return Vec::new();
        };
        let restored = self.restore_pending_contents(&pending, kept);
        let mut lines: Vec<_> = errors
            .iter()
            .map(|error| line(LineKind::Error, format!("error: {error}")))
            .collect();
        lines.push(line(
            LineKind::Error,
            if restored {
                "the message was not sent; it is back in the input"
            } else {
                "the message was not sent; newer input was left unchanged"
            },
        ));
        self.submit_state = SubmitState::Editing;
        vec![Effect::Append(lines)]
    }

    pub(super) fn on_submit_dispatch_finished(
        &mut self,
        intent_id: IntentId,
        result: SubmitDispatchResult,
    ) -> Vec<Effect> {
        match result {
            SubmitDispatchResult::Enqueued(request_id) => {
                if let Some(pending) = self.submit_state.pending_mut()
                    && pending.intent_id == intent_id
                {
                    pending.request_id = Some(request_id);
                }
                Vec::new()
            }
            SubmitDispatchResult::Backpressured => {
                self.fail_pending_submit(
                    intent_id,
                    line(
                        LineKind::Notice,
                        "服务繁忙，提交未发送",
                    ),
                )
            }
            SubmitDispatchResult::Disconnected { lane } => self.fail_pending_submit(
                intent_id,
                line(
                    LineKind::Error,
                    format!("error: frontend disconnected ({lane}); submit not sent"),
                ),
            ),
        }
    }

    pub(super) fn on_submit_command_rejected(
        &mut self,
        intent_id: IntentId,
        reason: CommandReject,
    ) -> Vec<Effect> {
        self.fail_pending_submit(
            intent_id,
            line(LineKind::Error, format!("error: {reason}")),
        )
    }

    pub(super) fn on_submit_accepted(
        &mut self,
        intent_id: IntentId,
        operation_id: String,
    ) -> Vec<Effect> {
        let Some(pending) = self.take_pending_if(intent_id) else {
            return Vec::new();
        };
        let mut rows: Vec<String> = pending.draft.split('\n').map(str::to_owned).collect();
        for attachment in &pending.attachments {
            rows.push(format!("[attached {}]", attachment.label()));
        }
        let lines = crate::app::transcript::user_block(rows);
        self.submit_state = SubmitState::Accepted {
            intent_id,
            operation_id,
        };
        vec![Effect::Append(lines)]
    }

    fn fail_pending_submit(
        &mut self,
        intent_id: IntentId,
        notice: crate::app::transcript::TranscriptLine,
    ) -> Vec<Effect> {
        let Some(pending) = self.take_pending_if(intent_id) else {
            return Vec::new();
        };
        let restored = self.restore_pending_contents(&pending, pending.attachments.clone());
        self.submit_state = SubmitState::Editing;
        let mut lines = vec![notice];
        if !restored {
            lines.push(line(
                LineKind::Notice,
                "submit was not sent; draft left as edited",
            ));
        }
        vec![Effect::Append(lines)]
    }

    fn take_pending_if(&mut self, intent_id: IntentId) -> Option<PendingSubmission> {
        match &self.submit_state {
            SubmitState::Dispatching(pending) if pending.intent_id == intent_id => {
                let SubmitState::Dispatching(pending) =
                    std::mem::replace(&mut self.submit_state, SubmitState::Editing)
                else {
                    unreachable!()
                };
                Some(pending)
            }
            _ => None,
        }
    }

    /// Restores pending draft when the editor was not edited after dispatch began.
    fn restore_pending_contents(
        &mut self,
        pending: &PendingSubmission,
        attachments: Vec<PendingAttachment>,
    ) -> bool {
        if self.draft_generation != pending.held_generation {
            return false;
        }
        self.restore_draft(&pending.draft, attachments);
        true
    }

    fn allocate_intent_id(&mut self) -> IntentId {
        let id = self.next_intent_id;
        self.next_intent_id = self.next_intent_id.wrapping_add(1).max(1);
        id
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
