//! Overlay key handling: approval answers and session-picker navigation.

use philo_session::SessionId;

use super::App;
use super::line;
use crate::api::confirmation::{ConfirmationId, ConfirmationRequest, ConfirmationResponse};
use crate::app::action::Action;
use crate::app::effect::{Effect, HostRequest};
use crate::app::overlay::{ConfirmPrompt, Preview, SessionPicker};
use crate::app::transcript::LineKind;

impl App {
    /// Keeps the approval overlay in step with the channel: the front
    /// request opens it, and a vanished one (answered, or auto-denied when
    /// the operation settled) closes it.
    pub fn sync_confirmation(
        &mut self,
        front: Option<(ConfirmationId, ConfirmationRequest)>,
    ) -> bool {
        let previous = self.confirm.as_ref().map(|prompt| prompt.id);
        match front {
            Some((id, request)) => {
                if self.confirm.as_ref().is_none_or(|prompt| prompt.id != id) {
                    self.confirm = Some(ConfirmPrompt::new(id, request));
                }
            }
            None => self.confirm = None,
        }
        previous != self.confirm.as_ref().map(|prompt| prompt.id)
    }

    pub(crate) fn open_picker(&mut self, sessions: Vec<SessionId>) {
        self.picker = Some(SessionPicker::new(sessions));
    }

    pub(crate) fn claim_preview(&mut self) -> Option<SessionId> {
        self.picker.as_mut()?.claim_preview()
    }

    pub(crate) fn set_preview(&mut self, session_id: &SessionId, preview: Preview) {
        if let Some(picker) = self.picker.as_mut() {
            picker.set_preview(session_id, preview);
        }
    }

    /// The open session picker, for tests and rendering.
    #[cfg(test)]
    pub(crate) fn picker(&self) -> Option<&SessionPicker> {
        self.picker.as_ref()
    }

    /// The open approval prompt, for tests and rendering.
    #[cfg(test)]
    pub(crate) fn confirm_prompt(&self) -> Option<&ConfirmPrompt> {
        self.confirm.as_ref()
    }

    pub(super) fn on_confirm_action(&mut self, action: Action) -> Vec<Effect> {
        let prompt = self.confirm.as_ref().expect("approval overlay is open");
        let (id, title) = (prompt.id, prompt.title().to_owned());
        let (response, verb) = match action {
            Action::InsertChar('y' | 'Y') => (ConfirmationResponse::Allow, "allowed"),
            Action::InsertChar('n' | 'N') | Action::Escape | Action::CtrlC => {
                (ConfirmationResponse::Deny, "denied")
            }
            _ => return vec![],
        };
        self.confirm = None;
        vec![
            Effect::Append(vec![line(LineKind::Meta, format!("{verb}: {title}"))]),
            Effect::Host(HostRequest::Respond(id, response)),
        ]
    }

    pub(super) fn on_picker_action(&mut self, action: Action) -> Vec<Effect> {
        let has_activity = self.has_activity();
        let picker = self.picker.as_mut().expect("session picker is open");
        match action {
            Action::MoveUp | Action::MoveDown => {
                let moved = if matches!(action, Action::MoveUp) {
                    picker.move_up()
                } else {
                    picker.move_down()
                };
                if !moved {
                    return vec![];
                }
                self.claim_preview()
                    .map(|id| vec![Effect::Host(HostRequest::LoadPreview(id))])
                    .unwrap_or_default()
            }
            Action::Submit => {
                if has_activity {
                    return vec![Effect::Append(vec![line(
                        LineKind::Error,
                        "error: the agent is still active; cancel it with Esc before switching \
                         sessions",
                    )])];
                }
                let selected = picker.selected().clone();
                self.picker = None;
                vec![Effect::Host(HostRequest::SwitchSession(selected))]
            }
            Action::Escape | Action::CtrlC => {
                self.picker = None;
                vec![]
            }
            _ => vec![],
        }
    }
}
