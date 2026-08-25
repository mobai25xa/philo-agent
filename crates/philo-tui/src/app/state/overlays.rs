//! Overlay key handling: approval answers and picker navigation/filtering.

use philo_agent_service::ConfirmationDecision;

use super::App;
use super::line;
use crate::app::action::Action;
use crate::app::effect::{Effect, HostRequest};
use crate::app::overlay::{ConfirmPrompt, Picker, PickerEntry, Preview};
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

    pub(crate) fn open_picker(&mut self, entries: Vec<PickerEntry>) {
        self.picker = Some(Picker::new("sessions", false, entries));
    }

    pub(crate) fn open_model_picker(&mut self, entries: Vec<PickerEntry>) {
        self.picker = Some(Picker::new("models", true, entries));
    }

    /// Whether the open picker lists models (drives Enter semantics and
    /// preview suppression).
    pub(crate) fn is_models_picker(&self) -> bool {
        self.picker
            .as_ref()
            .is_some_and(|picker| picker.is_models())
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
    pub(crate) fn picker(&self) -> Option<&Picker> {
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
        // Filter editing first: printable keys belong to the picker query,
        // never to the composer draft underneath.
        match action {
            Action::InsertChar(ch) => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.push_filter(ch);
                }
                return self.refresh_picker_preview();
            }
            Action::Backspace => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.pop_filter();
                }
                return Vec::new();
            }
            _ => {}
        }
        let has_activity = self.has_activity();
        let is_models = self.is_models_picker();
        match action {
            Action::MoveUp | Action::MoveDown => {
                let moved = self.picker.as_mut().is_some_and(|picker| {
                    if matches!(action, Action::MoveUp) {
                        picker.move_up()
                    } else {
                        picker.move_down()
                    }
                });
                if !moved || is_models {
                    return Vec::new();
                }
                self.refresh_picker_preview()
            }
            Action::Home | Action::End => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.jump(matches!(action, Action::Home));
                }
                Vec::new()
            }
            Action::Submit => {
                let Some(selected) = self
                    .picker
                    .as_ref()
                    .map(|picker| picker.selected().to_owned())
                else {
                    return Vec::new();
                };
                if is_models {
                    self.picker = None;
                    self.pending_model_switch = true;
                    return vec![
                        Effect::Append(vec![line(
                            LineKind::Meta,
                            format!("switching model to {selected}..."),
                        )]),
                        Effect::Host(HostRequest::RebuildModel(selected)),
                    ];
                }
                if has_activity {
                    return vec![Effect::Append(vec![line(
                        LineKind::Error,
                        "error: the agent is still active; cancel it with Esc before switching \
                         sessions",
                    )])];
                }
                self.picker = None;
                self.session_load_intent = Some(SessionLoadIntent::Switch);
                vec![Effect::Host(HostRequest::SwitchSession(selected))]
            }
            Action::Escape | Action::CtrlC => {
                self.picker = None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    pub(crate) fn refresh_picker_preview(&mut self) -> Vec<Effect> {
        if self.is_models_picker() {
            return vec![];
        }
        self.claim_preview()
            .map(|id| vec![Effect::Host(HostRequest::LoadPreview(id))])
            .unwrap_or_default()
    }
}

/// How the next `SessionLoaded` should be presented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionLoadIntent {
    New,
    Switch,
    Snapshot,
}
