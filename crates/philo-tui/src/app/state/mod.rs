//! The interaction state machine: key actions and agent events go in,
//! append-only transcript lines and side-effect requests come out. Pure
//! state — the event loop owns the terminal and the host.

mod commands;
mod composer;
mod overlays;
mod runtime;

#[cfg(test)]
mod tests;

use super::action::Action;
use super::activity::{ActivityState, ActivityTone, ActivityView};
use super::attachment::Attachments;
use super::effect::Effect;
use super::input::{InputEditor, InputHistory};
use super::overlay::{ConfirmPrompt, OverlayFrame, SessionPicker};
use super::status::StatusData;
use super::transcript::{InfoLevel, LineKind, Transcript, TranscriptLine};

use commands::CompletionMenu;

/// Pure interaction state for one TUI session.
pub(crate) struct App {
    pub(crate) input: InputEditor,
    pub(crate) transcript: Transcript,
    pub(crate) status: StatusData,
    level: InfoLevel,
    history: InputHistory,
    /// One `Ctrl+C` on an idle, empty prompt arms the exit; the second
    /// quits. Any other action disarms.
    exit_armed: bool,
    /// `/quit` during a running turn asks once before leaving.
    quit_armed: bool,
    /// The session picker, while `/sessions` is open.
    picker: Option<SessionPicker>,
    /// The approval prompt, while a confirmation request is pending.
    confirm: Option<ConfirmPrompt>,
    /// The command-completion menu, while Tab is cycling.
    completion: Option<CompletionMenu>,
    /// Images waiting for the next message (`/image`, `Ctrl+V`).
    attachments: Attachments,
    /// Changes whenever draft contents are consumed or edited. Background
    /// media failures may restore only the exact draft generation they left.
    draft_generation: u64,
    /// `[ui].show_reasoning`, carried across session switches.
    show_reasoning: bool,
    /// Manual compaction has a standalone future owned by the driver.
    manual_compacting: bool,
    /// Automatic compaction belongs to the front operation handle.
    automatic_compacting: bool,
    /// Ephemeral operation projection; never enters transcript or Session.
    activity: ActivityState,
}

impl App {
    pub fn new(status: StatusData, show_reasoning: bool) -> Self {
        let level = status.level;
        Self {
            input: InputEditor::new(),
            transcript: Transcript::new(show_reasoning),
            status,
            level,
            history: InputHistory::default(),
            exit_armed: false,
            quit_armed: false,
            picker: None,
            confirm: None,
            completion: None,
            attachments: Attachments::default(),
            draft_generation: 0,
            show_reasoning,
            manual_compacting: false,
            automatic_compacting: false,
            activity: ActivityState::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn level(&self) -> InfoLevel {
        self.level
    }

    #[cfg(test)]
    pub(crate) fn shows_reasoning(&self) -> bool {
        self.show_reasoning
    }

    /// Images waiting for the next message (`/image`, `Ctrl+V`).
    pub fn attachments(&self) -> &Attachments {
        &self.attachments
    }

    /// The overlay content to paint, if any. The approval prompt wins over
    /// the session picker: an answer is what unblocks the running turn.
    #[cfg(test)]
    pub fn overlay_frame(&self, height: usize) -> Option<OverlayFrame> {
        if let Some(confirm) = &self.confirm {
            return Some(confirm.frame(height));
        }
        self.picker.as_ref().map(|picker| picker.frame(height))
    }

    pub(crate) fn overlay_frame_for(&self, height: usize, width: usize) -> Option<OverlayFrame> {
        if let Some(confirm) = &self.confirm {
            return Some(confirm.frame_for(height, width));
        }
        self.picker
            .as_ref()
            .map(|picker| picker.frame_for(height, width))
    }

    pub(crate) fn activity_view(&self, width: usize) -> Option<ActivityView> {
        if self.confirm.is_some() {
            return Some(ActivityView {
                text: super::text::truncate("! Approval required", width),
                tone: ActivityTone::Warning,
            });
        }
        self.activity.view(width)
    }

    pub(crate) fn activity_detail_rows(&self, width: usize, height: usize) -> Vec<String> {
        if self.confirm.is_some() || self.picker.is_some() {
            return Vec::new();
        }
        let tail_lines = match self.level {
            InfoLevel::Verbose => 20,
            InfoLevel::Default => 5,
        };
        self.activity.detail_rows(width, height, tail_lines)
    }

    pub(crate) fn activity_timeline_row(&self, width: usize) -> Option<String> {
        if self.confirm.is_some() || self.picker.is_some() {
            return None;
        }
        self.activity.timeline_row(width)
    }

    pub(crate) fn has_overlay(&self) -> bool {
        self.confirm.is_some() || self.picker.is_some()
    }

    pub(crate) fn input_focused(&self) -> bool {
        self.confirm.is_none() && self.picker.is_none()
    }

    /// The completion menu row, while it is open.
    pub fn completion_line(&self) -> Option<String> {
        self.completion.as_ref().map(CompletionMenu::line)
    }

    /// Handles one interpreted key action.
    pub fn on_action(&mut self, action: Action) -> Vec<Effect> {
        if let Action::ConfigReload(notice) = action {
            return self.apply_config_notice(notice);
        }
        if self.confirm.is_some() {
            return self.on_confirm_action(action);
        }
        if self.picker.is_some() {
            return self.on_picker_action(action);
        }
        if self.completion.is_some() && matches!(action, Action::Escape) {
            self.completion = None;
            return vec![];
        }
        // Any interaction other than the quit chord disarms the two-step
        // exit; anything but another `/quit` disarms the running-turn exit.
        if !matches!(action, Action::CtrlC) {
            self.exit_armed = false;
        }
        if !matches!(action, Action::Complete) {
            self.completion = None;
        }
        match action {
            Action::InsertChar(ch) => self.insert_char(ch),
            Action::InsertNewline => self.insert_newline(),
            Action::Backspace => self.backspace(),
            Action::Delete => self.delete(),
            Action::MoveLeft => self.move_left(),
            Action::MoveRight => self.move_right(),
            Action::Home => self.home(),
            Action::End => self.end(),
            Action::MoveUp => self.move_up(),
            Action::MoveDown => self.move_down(),
            Action::Submit => self.submit(),
            Action::Escape => self.escape(),
            Action::CtrlC => self.ctrl_c(),
            Action::CtrlD => {
                if self.input.is_empty() {
                    vec![Effect::Quit]
                } else {
                    vec![]
                }
            }
            Action::ToggleLevel => vec![Effect::Append(vec![self.toggle_level()])],
            Action::Redraw => vec![Effect::HardRedraw],
            Action::Complete => self.complete(),
            Action::Paste => vec![Effect::ReadClipboard],
            Action::ConfigReload(_) => unreachable!("config notices are handled first"),
            Action::None => vec![],
        }
    }

    fn has_activity(&self) -> bool {
        self.status.busy || self.status.compacting
    }

    fn bump_draft_generation(&mut self) {
        self.draft_generation = self.draft_generation.wrapping_add(1);
    }
}

fn line(kind: LineKind, text: impl Into<String>) -> TranscriptLine {
    TranscriptLine {
        kind,
        text: text.into(),
    }
}
