//! Overlay key handling: approval answers and picker navigation/filtering.

use philo_agent_service::ConfirmationDecision;

use super::App;
use super::line;
use crate::app::action::Action;
use crate::app::command;
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
        self.picker = Some(Picker::new("Sessions", false, entries));
    }

    pub(crate) fn open_model_picker(&mut self, entries: Vec<PickerEntry>) {
        let mut picker = Picker::new("Models", true, entries);
        picker.set_current_effort(self.status.effort.clone());
        self.picker = Some(picker);
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
        let is_models = self.is_models_picker();
        let in_tier_mode = self
            .picker
            .as_ref()
            .is_some_and(crate::app::overlay::Picker::in_tier_mode);
        // Filter editing only on the model/session level: printable keys
        // belong to the picker query, never to the composer draft.
        match action {
            Action::InsertChar(ch) if !in_tier_mode => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.push_filter(ch);
                }
                return self.refresh_picker_preview();
            }
            Action::Backspace if !in_tier_mode => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.pop_filter();
                }
                return Vec::new();
            }
            _ => {}
        }
        let has_activity = self.has_activity();
        match action {
            Action::MoveUp | Action::MoveDown => {
                let moved = self.picker.as_mut().is_some_and(|picker| {
                    let up = matches!(action, Action::MoveUp);
                    if picker.in_tier_mode() {
                        picker.move_tier(up)
                    } else if up {
                        picker.move_up()
                    } else {
                        picker.move_down()
                    }
                });
                if !moved || is_models || in_tier_mode {
                    return Vec::new();
                }
                self.refresh_picker_preview()
            }
            Action::Home | Action::End => {
                if let Some(picker) = self.picker.as_mut() {
                    let to_top = matches!(action, Action::Home);
                    if picker.in_tier_mode() {
                        picker.jump_tier(to_top);
                    } else {
                        picker.jump(to_top);
                    }
                }
                Vec::new()
            }
            // `tab` flips between the model list and its reasoning tiers
            // (v0.37 §4.2); it stays inert for session pickers.
            Action::Complete if is_models => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.toggle_tier_mode();
                }
                Vec::new()
            }
            Action::Submit => {
                if is_models {
                    return self.submit_models_picker();
                }
                let Some(selected) = self
                    .picker
                    .as_ref()
                    .map(|picker| picker.selected().to_owned())
                else {
                    return Vec::new();
                };
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
                // Esc backs out of the tier level one step first; CtrlC (or
                // Esc on the model level) closes the whole picker.
                if is_models && in_tier_mode && matches!(action, Action::Escape) {
                    if let Some(picker) = self.picker.as_mut() {
                        picker.toggle_tier_mode();
                    }
                } else {
                    self.picker = None;
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Enter on the models picker: on the model level a tier-less model
    /// installs directly while a tiered model steps into the tier level; on
    /// the tier level Enter confirms the `model × tier` choice. The current
    /// model with a new tier takes the synchronous `SetReasoning` path; a
    /// different model installs atomically with the chosen tier frozen in.
    fn submit_models_picker(&mut self) -> Vec<Effect> {
        let Some(picker) = self.picker.as_ref() else {
            return Vec::new();
        };
        if !picker.in_tier_mode() {
            let Some(selected) = self
                .picker
                .as_ref()
                .map(|picker| picker.selected().to_owned())
            else {
                return Vec::new();
            };
            let has_tiers = picker.highlighted_has_tiers();
            if has_tiers {
                if let Some(open) = self.picker.as_mut() {
                    open.toggle_tier_mode();
                }
                return Vec::new();
            }
            self.picker = None;
            self.pending_model_switch = true;
            return vec![
                Effect::Append(vec![line(
                    LineKind::Meta,
                    format!("switching model to {selected}..."),
                )]),
                Effect::Host(HostRequest::RebuildModel {
                    name: selected,
                    effort: None,
                }),
            ];
        }

        let Some((model_id, tier)) = picker.selected_tier() else {
            return Vec::new();
        };
        let tier_is_current = picker.selected_tier_is_current();
        let model_is_current = picker.selected_is_current();
        self.picker = None;
        let Some(effort) = command::parse_reasoning(&tier) else {
            // Catalog labels always parse; this guards drift only.
            return vec![Effect::Append(vec![line(
                LineKind::Error,
                format!("unknown reasoning tier: {tier}"),
            )])];
        };
        if model_is_current {
            if tier_is_current {
                return vec![Effect::Append(vec![line(
                    LineKind::Meta,
                    format!("reasoning already {tier}"),
                )])];
            }
            return vec![Effect::Host(HostRequest::SetReasoning(effort))];
        }
        self.pending_model_switch = true;
        vec![
            Effect::Append(vec![line(
                LineKind::Meta,
                format!("switching model to {model_id} (reasoning {tier})..."),
            )]),
            Effect::Host(HostRequest::RebuildModel {
                name: model_id,
                effort: Some(effort),
            }),
        ]
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
