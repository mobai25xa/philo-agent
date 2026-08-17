//! Overlay key handling: approval answers and session-picker navigation.

use philo_agent_service::ConfirmationDecision;

use super::App;
use super::line;
use crate::app::action::Action;
use crate::app::effect::{Effect, HostRequest};
use crate::app::overlay::{ConfirmPrompt, Preview, SessionPicker};
use crate::app::transcript::LineKind;

impl App {
    /// Keeps the approval overlay in step with the front pending
    /// confirmation. A vanished id (answered or auto-denied) closes it.
    pub fn sync_confirmation(&mut self, front: Option<(u64, String, String)>) -> bool {
        let previous = self.confirm.as_ref().map(|prompt| prompt.id);
        match front {
            Some((id, title, body)) => {
                if self.confirm.as_ref().is_none_or(|prompt| prompt.id != id) {
                    self.confirm = Some(ConfirmPrompt::new(id, title, body));
                }
            }
            None => self.confirm = None,
        }
        previous != self.confirm.as_ref().map(|prompt| prompt.id)
    }

    pub(crate) fn open_picker(&mut self, sessions: Vec<String>) {
        self.picker = Some(SessionPicker::new(sessions));
    }

    pub(crate) fn claim_preview(&mut self) -> Option<String> {
        self.picker.as_mut()?.claim_preview()
    }

    pub(crate) fn set_preview(&mut self, session_id: &str, preview: Preview) {
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
        let (decision, verb) = match action {
            Action::InsertChar('y' | 'Y') => (ConfirmationDecision::Allow, "allowed"),
            Action::InsertChar('n' | 'N') | Action::Escape | Action::CtrlC => {
                (ConfirmationDecision::Deny, "denied")
            }
            _ => return vec![],
        };
        self.confirm = None;
        vec![
            Effect::Append(vec![line(LineKind::Meta, format!("{verb}: {title}"))]),
            Effect::Host(HostRequest::Respond(id, decision)),
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
                let selected = picker.selected().to_owned();
                self.picker = None;
                self.session_load_intent = Some(SessionLoadIntent::Switch);
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

/// How the next `SessionLoaded` should be presented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionLoadIntent {
    New,
    Switch,
    Snapshot,
}
