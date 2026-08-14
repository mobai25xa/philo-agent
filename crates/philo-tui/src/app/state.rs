//! The interaction state machine: key actions and agent events go in,
//! append-only transcript lines and side-effect requests come out. Pure
//! state — the event loop owns the terminal and the host.

use philo_agent_runtime::{AgentAvailability, AgentEvent, CompactionError, CompactionReport};
use philo_session::SessionId;

use crate::api::confirmation::{ConfirmationId, ConfirmationRequest, ConfirmationResponse};

use super::action::Action;
use super::attachment::{Attachments, PendingAttachment};
use super::command::{self, Command};
use super::effect::{Effect, HostRequest};
use super::input::InputEditor;
use super::overlay::{ConfirmPrompt, OverlayFrame, Preview, SessionPicker};
use super::status::StatusData;
use super::transcript::{InfoLevel, LineKind, Transcript, TranscriptLine};

/// An open command-completion menu: the candidates and the cycling cursor
/// (`None` while the input still holds the shared prefix).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletionMenu {
    candidates: Vec<&'static str>,
    selected: Option<usize>,
}

impl CompletionMenu {
    fn new(candidates: Vec<&'static str>) -> Self {
        Self {
            candidates,
            selected: None,
        }
    }

    /// Advances to the next candidate and returns its name.
    fn cycle(&mut self) -> &'static str {
        let next = match self.selected {
            None => 0,
            Some(index) => (index + 1) % self.candidates.len(),
        };
        self.selected = Some(next);
        self.candidates[next]
    }

    /// The single row shown above the input while the menu is open.
    pub fn line(&self) -> String {
        let names: Vec<String> = self
            .candidates
            .iter()
            .enumerate()
            .map(|(index, name)| {
                if self.selected == Some(index) {
                    format!("[{name}]")
                } else {
                    (*name).to_owned()
                }
            })
            .collect();
        format!("commands: {}", names.join(" "))
    }
}

/// Pure interaction state for one TUI session.
pub(crate) struct App {
    pub(crate) input: InputEditor,
    pub(crate) transcript: Transcript,
    pub(crate) status: StatusData,
    level: InfoLevel,
    history: Vec<String>,
    /// Index into `history` while browsing; `None` when editing fresh text.
    history_cursor: Option<usize>,
    /// The fresh text stashed while browsing history.
    stash: Option<String>,
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
    /// `[ui].show_reasoning`, carried across session switches.
    show_reasoning: bool,
    /// Manual compaction has a standalone future owned by the driver.
    manual_compacting: bool,
    /// Automatic compaction belongs to the front operation handle.
    automatic_compacting: bool,
}

impl App {
    pub fn new(status: StatusData, show_reasoning: bool) -> Self {
        let level = status.level;
        Self {
            input: InputEditor::new(),
            transcript: Transcript::new(show_reasoning),
            status,
            level,
            history: Vec::new(),
            history_cursor: None,
            stash: None,
            exit_armed: false,
            quit_armed: false,
            picker: None,
            confirm: None,
            completion: None,
            attachments: Attachments::default(),
            show_reasoning,
            manual_compacting: false,
            automatic_compacting: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn level(&self) -> InfoLevel {
        self.level
    }

    /// Images waiting for the next message (`/image`, `Ctrl+V`).
    pub fn attachments(&self) -> &Attachments {
        &self.attachments
    }

    /// The event loop keeps the busy flag current (handles outstanding).
    pub fn set_busy(&mut self, busy: bool, queued: usize) {
        self.status.busy = busy;
        self.status.queued = queued;
        if !busy {
            self.automatic_compacting = false;
            self.sync_compacting_status();
        }
    }

    /// Advances the manual/automatic compaction spinner on redraw ticks.
    pub(crate) fn on_tick(&mut self) -> Vec<Effect> {
        self.status.advance_spinner();
        vec![Effect::Redraw]
    }

    /// Applies the terminal result of the driver's manual compaction future.
    pub(crate) fn finish_manual_compaction(
        &mut self,
        result: Result<CompactionReport, CompactionError>,
    ) -> Vec<Effect> {
        self.manual_compacting = false;
        self.sync_compacting_status();
        let line = match result {
            Ok(CompactionReport::Compacted { covers_up_to }) => {
                self.status.usage = None;
                line(
                    LineKind::Meta,
                    format!("context compacted through {covers_up_to}"),
                )
            }
            Ok(CompactionReport::NothingToCompact) => line(
                LineKind::Notice,
                "nothing to compact: no older completed turns are available",
            ),
            Err(CompactionError::Unavailable { availability }) => {
                let reason = match availability {
                    AgentAvailability::Busy { .. } => "a turn is already running",
                    AgentAvailability::Compacting { .. } => "context compaction is already running",
                    AgentAvailability::Idle => "the runtime refused maintenance",
                };
                line(
                    LineKind::Error,
                    format!("error: context compaction was not started: {reason}"),
                )
            }
            Err(error) => line(
                LineKind::Error,
                format!("error: context compaction failed: {}", error.message()),
            ),
        };
        vec![Effect::Append(vec![line])]
    }

    /// The overlay content to paint, if any. The approval prompt wins over
    /// the session picker: an answer is what unblocks the running turn.
    pub fn overlay_frame(&self, height: usize) -> Option<OverlayFrame> {
        if let Some(confirm) = &self.confirm {
            return Some(confirm.frame(height));
        }
        self.picker.as_ref().map(|picker| picker.frame(height))
    }

    /// The completion menu row, while it is open.
    pub fn completion_line(&self) -> Option<String> {
        self.completion.as_ref().map(CompletionMenu::line)
    }

    /// Keeps the approval overlay in step with the channel: the front
    /// request opens it, and a vanished one (answered, or auto-denied when
    /// the operation settled) closes it.
    pub fn sync_confirmation(&mut self, front: Option<(ConfirmationId, ConfirmationRequest)>) {
        match front {
            Some((id, request)) => {
                if self.confirm.as_ref().is_none_or(|prompt| prompt.id != id) {
                    self.confirm = Some(ConfirmPrompt::new(id, request));
                }
            }
            None => self.confirm = None,
        }
    }

    /// Starts rendering a different session: fresh transcript and usage,
    /// same terminal scrollback (history is append-only).
    pub(crate) fn begin_session(&mut self, session_id: &str) {
        self.status.session = session_id.to_owned();
        self.status.usage = None;
        self.transcript = Transcript::new(self.show_reasoning);
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

    /// Handles one interpreted key action.
    pub fn on_action(&mut self, action: Action) -> Vec<Effect> {
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
            Action::InsertChar(ch) => {
                self.history_cursor = None;
                self.input.insert_char(ch);
                self.disarm_quit_unless_typing_quit();
                vec![]
            }
            Action::InsertNewline => {
                self.history_cursor = None;
                self.input.insert_newline();
                self.disarm_quit_unless_typing_quit();
                vec![]
            }
            Action::Backspace => {
                self.input.backspace();
                self.disarm_quit_unless_typing_quit();
                vec![]
            }
            Action::Delete => {
                self.input.delete();
                self.disarm_quit_unless_typing_quit();
                vec![]
            }
            Action::MoveLeft => {
                self.input.move_left();
                vec![]
            }
            Action::MoveRight => {
                self.input.move_right();
                vec![]
            }
            Action::Home => {
                self.input.home();
                vec![]
            }
            Action::End => {
                self.input.end();
                vec![]
            }
            Action::MoveUp => {
                if !self.input.move_up() {
                    self.history_prev();
                }
                vec![]
            }
            Action::MoveDown => {
                if !self.input.move_down() {
                    self.history_next();
                }
                vec![]
            }
            Action::Submit => self.submit(),
            Action::Escape => {
                if self.manual_compacting {
                    self.cancel_manual_compaction()
                } else if self.status.busy {
                    vec![Effect::CancelActive]
                } else {
                    vec![]
                }
            }
            Action::CtrlC => self.ctrl_c(),
            Action::CtrlD => {
                if self.input.is_empty() {
                    vec![Effect::Quit]
                } else {
                    vec![]
                }
            }
            Action::ToggleLevel => vec![Effect::Append(vec![self.toggle_level()])],
            Action::Redraw => vec![Effect::Redraw],
            Action::Complete => self.complete(),
            Action::Paste => vec![Effect::ReadClipboard],
            Action::None => vec![],
        }
    }

    /// Pastes text verbatim (bracketed paste never submits).
    pub fn on_paste(&mut self, text: &str) -> Vec<Effect> {
        if self.confirm.is_some() || self.picker.is_some() {
            return vec![];
        }
        self.exit_armed = false;
        self.completion = None;
        self.history_cursor = None;
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
        self.input.set_text(text);
        self.attachments.extend(attachments);
    }

    /// Projects one agent event into transcript lines and status updates.
    /// Overlays never intercept this path: terminal events must render.
    pub fn on_agent_event(&mut self, event: &AgentEvent) -> Vec<Effect> {
        match event {
            AgentEvent::ModelUsageUpdated { usage, .. } => {
                self.status.usage = Some(*usage);
            }
            AgentEvent::ContextCompactionStarted => {
                self.automatic_compacting = true;
                self.sync_compacting_status();
            }
            AgentEvent::ContextCompactionCompleted { .. } => {
                self.automatic_compacting = false;
                self.status.usage = None;
                self.sync_compacting_status();
            }
            AgentEvent::ContextCompactionFailed { .. } => {
                self.automatic_compacting = false;
                self.sync_compacting_status();
            }
            _ => {}
        }
        let lines = self.transcript.on_event(event, self.level);
        if lines.is_empty() {
            vec![]
        } else {
            vec![Effect::Append(lines)]
        }
    }

    fn on_confirm_action(&mut self, action: Action) -> Vec<Effect> {
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

    fn on_picker_action(&mut self, action: Action) -> Vec<Effect> {
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

    /// Tab: completes the command word. A single candidate completes
    /// outright; several open the menu at their shared prefix and each
    /// further Tab cycles through them.
    fn complete(&mut self) -> Vec<Effect> {
        if let Some(menu) = self.completion.as_mut() {
            let name = menu.cycle();
            self.input.set_text(&format!("/{name}"));
            return vec![];
        }
        let candidates: Vec<&'static str> = command::candidates(&self.input.text())
            .iter()
            .map(|spec| spec.name)
            .collect();
        match candidates.len() {
            0 => {}
            1 => self.input.set_text(&format!("/{} ", candidates[0])),
            _ => {
                self.input
                    .set_text(&format!("/{}", command::common_prefix(&candidates)));
                self.completion = Some(CompletionMenu::new(candidates));
            }
        }
        vec![]
    }

    fn submit(&mut self) -> Vec<Effect> {
        if self.input.is_empty() {
            return vec![];
        }
        let text = self.input.take_text();
        self.completion = None;
        self.history_cursor = None;
        self.stash = None;
        self.history.push(text.clone());

        // A `/` prefix is a command: it never reaches the model.
        if text.starts_with('/') {
            return self.run_command(&text);
        }
        self.quit_armed = false;

        let mut lines = vec![line(
            LineKind::User,
            format!("> {}", text.replace('\n', "\n> ")),
        )];
        let attachments = self.attachments.take();
        for attachment in &attachments {
            lines.push(line(
                LineKind::User,
                format!("> [attached {}]", attachment.label()),
            ));
        }
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

    #[allow(clippy::too_many_lines)]
    fn run_command(&mut self, text: &str) -> Vec<Effect> {
        // Only a repeated `/quit` stays armed.
        let quit_armed = std::mem::take(&mut self.quit_armed);
        let mut lines = vec![line(LineKind::Meta, text.to_owned())];
        let mut effects: Vec<Effect> = Vec::new();

        match command::parse(text) {
            Err(unknown) => lines.push(line(
                LineKind::Error,
                format!("unknown command: /{} (try /help)", unknown.word()),
            )),
            Ok(Command::Help) => lines.extend(
                command::help_lines()
                    .into_iter()
                    .map(|text| line(LineKind::Meta, text)),
            ),
            Ok(Command::New) => {
                if self.has_activity() {
                    lines.push(line(
                        LineKind::Error,
                        "error: the agent is still active; cancel it with Esc before starting a \
                         new session",
                    ));
                } else {
                    effects.push(Effect::Host(HostRequest::NewSession));
                }
            }
            Ok(Command::Sessions) => effects.push(Effect::Host(HostRequest::OpenSessions)),
            Ok(Command::Model { name: None }) => {
                lines.push(line(LineKind::Error, "usage: /model <name>"));
            }
            Ok(Command::Model { name: Some(name) }) => {
                if self.has_activity() {
                    lines.push(line(
                        LineKind::Error,
                        format!(
                            "error: the model can only be switched while idle; still on {}",
                            self.status.model
                        ),
                    ));
                } else {
                    effects.push(Effect::Host(HostRequest::RebuildModel(name)));
                }
            }
            Ok(Command::Reasoning { level: None }) => {
                lines.push(line(
                    LineKind::Error,
                    format!("usage: /reasoning <level> ({})", command::REASONING_LEVELS),
                ));
            }
            Ok(Command::Reasoning { level: Some(level) }) => {
                match command::parse_reasoning(&level) {
                    Some(effort) => effects.push(Effect::Host(HostRequest::SetReasoning(effort))),
                    None => lines.push(line(
                        LineKind::Error,
                        format!(
                            "unknown reasoning level: {level} (expected {})",
                            command::REASONING_LEVELS
                        ),
                    )),
                }
            }
            Ok(Command::Compact) => {
                if self.status.compacting {
                    lines.push(line(
                        LineKind::Error,
                        "error: context compaction is already running; press Esc to cancel it",
                    ));
                } else if self.status.busy {
                    lines.push(line(
                        LineKind::Error,
                        "error: context can only be compacted while idle; a turn is still \
                         running",
                    ));
                } else {
                    self.manual_compacting = true;
                    self.sync_compacting_status();
                    effects.push(Effect::StartCompaction);
                }
            }
            Ok(Command::Image { path: None }) => {
                lines.push(line(LineKind::Error, "usage: /image <path>"));
            }
            Ok(Command::Image { path: Some(path) }) => {
                self.attachments.push(PendingAttachment::Path(path.clone()));
                lines.push(line(
                    LineKind::Meta,
                    format!(
                        "image queued: {path} ({} waiting for the next message)",
                        self.attachments.len()
                    ),
                ));
            }
            Ok(Command::Verbose) => lines.push(self.toggle_level()),
            Ok(Command::Status) => effects.push(Effect::Host(HostRequest::ShowStatus)),
            Ok(Command::Config) => effects.push(Effect::Host(HostRequest::ShowConfig)),
            Ok(Command::Quit) => {
                if self.has_activity() && !quit_armed {
                    self.quit_armed = true;
                    lines.push(line(
                        LineKind::Notice,
                        "the agent is still active: press Esc to cancel it, or /quit again to \
                         leave anyway",
                    ));
                } else {
                    effects.push(Effect::Quit);
                }
            }
        }

        let mut result = vec![Effect::Append(lines)];
        result.append(&mut effects);
        result
    }

    fn toggle_level(&mut self) -> TranscriptLine {
        self.level = match self.level {
            InfoLevel::Default => InfoLevel::Verbose,
            InfoLevel::Verbose => InfoLevel::Default,
        };
        self.status.level = self.level;
        line(
            LineKind::Meta,
            format!(
                "info level: {}",
                if self.level == InfoLevel::Verbose {
                    "verbose"
                } else {
                    "default"
                }
            ),
        )
    }

    /// The running-turn `/quit` confirmation survives typing the literal
    /// second `/quit`, but any edit that can no longer become that command
    /// disarms it. This keeps the confirmation two-step without making an
    /// unrelated edit count as the second step.
    fn disarm_quit_unless_typing_quit(&mut self) {
        if self.quit_armed && !"/quit".starts_with(&self.input.text()) {
            self.quit_armed = false;
        }
    }

    fn ctrl_c(&mut self) -> Vec<Effect> {
        if !self.input.is_empty() {
            self.input.clear();
            self.history_cursor = None;
            self.exit_armed = false;
            return vec![];
        }
        if self.manual_compacting {
            self.exit_armed = false;
            return self.cancel_manual_compaction();
        }
        if self.status.busy {
            self.exit_armed = false;
            return vec![Effect::CancelActive];
        }
        if self.exit_armed {
            return vec![Effect::Quit];
        }
        self.exit_armed = true;
        vec![Effect::Append(vec![line(
            LineKind::Notice,
            "press Ctrl+C again to exit",
        )])]
    }

    fn has_activity(&self) -> bool {
        self.status.busy || self.status.compacting
    }

    fn sync_compacting_status(&mut self) {
        self.status
            .set_compacting(self.manual_compacting || self.automatic_compacting);
    }

    fn cancel_manual_compaction(&mut self) -> Vec<Effect> {
        self.manual_compacting = false;
        self.sync_compacting_status();
        vec![
            Effect::CancelCompaction,
            Effect::Append(vec![line(LineKind::Notice, "context compaction cancelled")]),
        ]
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next_index = match self.history_cursor {
            None => {
                self.stash = Some(self.input.text());
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(index) => index - 1,
        };
        self.history_cursor = Some(next_index);
        self.input.set_text(&self.history[next_index].clone());
    }

    fn history_next(&mut self) {
        let Some(index) = self.history_cursor else {
            return;
        };
        if index + 1 < self.history.len() {
            self.history_cursor = Some(index + 1);
            self.input.set_text(&self.history[index + 1].clone());
        } else {
            self.history_cursor = None;
            let stash = self.stash.take().unwrap_or_default();
            self.input.set_text(&stash);
        }
    }
}

fn line(kind: LineKind, text: impl Into<String>) -> TranscriptLine {
    TranscriptLine {
        kind,
        text: text.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App::new(StatusData::new("m", "s", InfoLevel::Default), true)
    }

    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.on_action(Action::InsertChar(ch));
        }
    }

    /// Submits `text` and returns the produced effects.
    fn run(app: &mut App, text: &str) -> Vec<Effect> {
        type_text(app, text);
        app.on_action(Action::Submit)
    }

    /// The transcript lines of the first Append effect.
    fn appended(effects: &[Effect]) -> Vec<TranscriptLine> {
        effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Append(lines) => Some(lines.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn texts(lines: &[TranscriptLine]) -> Vec<String> {
        lines.iter().map(|line| line.text.clone()).collect()
    }

    fn host_requests(effects: &[Effect]) -> Vec<HostRequest> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Host(request) => Some(request.clone()),
                _ => None,
            })
            .collect()
    }

    fn request(title: &str) -> ConfirmationRequest {
        ConfirmationRequest {
            title: title.to_owned(),
            body: format!("{title} body"),
        }
    }

    #[test]
    fn submit_echoes_the_message_and_requests_a_prompt() {
        let mut app = app();
        let effects = run(&mut app, "hello");
        assert_eq!(
            effects,
            vec![
                Effect::Append(vec![TranscriptLine {
                    kind: LineKind::User,
                    text: "> hello".to_owned(),
                }]),
                Effect::Submit {
                    text: "hello".to_owned(),
                    attachments: Vec::new(),
                },
            ]
        );
        assert!(app.input.is_empty());
    }

    #[test]
    fn empty_submit_is_a_no_op() {
        let mut app = app();
        assert!(app.on_action(Action::Submit).is_empty());
    }

    #[test]
    fn busy_submit_notes_the_queueing() {
        let mut app = app();
        app.set_busy(true, 0);
        let effects = run(&mut app, "next");
        let lines = appended(&effects);
        assert!(lines.iter().any(|line| line.text.contains("queued")));
        assert_eq!(
            effects[1],
            Effect::Submit {
                text: "next".to_owned(),
                attachments: Vec::new(),
            }
        );
    }

    #[test]
    fn unknown_commands_never_submit_to_the_model() {
        let mut app = app();
        let effects = run(&mut app, "/definitely-unknown");
        assert_eq!(effects.len(), 1, "echo and error only");
        let lines = appended(&effects);
        assert_eq!(lines[0].text, "/definitely-unknown", "the command echoes");
        assert_eq!(lines[1].kind, LineKind::Error);
        assert_eq!(
            lines[1].text,
            "unknown command: /definitely-unknown (try /help)"
        );
    }

    #[test]
    fn help_lists_the_whole_command_table() {
        let mut app = app();
        let lines = texts(&appended(&run(&mut app, "/help")));
        for spec in command::COMMANDS {
            assert!(
                lines.iter().any(|line| line.contains(spec.usage)),
                "missing {} in {lines:?}",
                spec.usage
            );
        }
    }

    #[test]
    fn help_snapshot() {
        let mut app = app();
        crate::tests::assert_tui_snapshot!(
            "app_help",
            texts(&appended(&run(&mut app, "/help"))).join("\n")
        );
    }

    #[test]
    fn new_and_sessions_go_through_the_host() {
        let mut app = app();
        assert_eq!(
            host_requests(&run(&mut app, "/new")),
            vec![HostRequest::NewSession]
        );
        assert_eq!(
            host_requests(&run(&mut app, "/sessions")),
            vec![HostRequest::OpenSessions]
        );
    }

    #[test]
    fn a_new_session_is_refused_while_a_turn_runs() {
        let mut app = app();
        app.set_busy(true, 0);
        let effects = run(&mut app, "/new");
        assert!(host_requests(&effects).is_empty());
        assert_eq!(appended(&effects)[1].kind, LineKind::Error);
    }

    #[test]
    fn model_needs_a_name_and_an_idle_runtime() {
        let mut app = app();
        let lines = appended(&run(&mut app, "/model"));
        assert_eq!(lines[1].text, "usage: /model <name>");

        app.set_busy(true, 0);
        let effects = run(&mut app, "/model other");
        assert!(host_requests(&effects).is_empty(), "busy refuses");
        assert!(appended(&effects)[1].text.contains("still on m"));

        app.set_busy(false, 0);
        assert_eq!(
            host_requests(&run(&mut app, "/model other")),
            vec![HostRequest::RebuildModel("other".to_owned())]
        );
    }

    #[test]
    fn reasoning_maps_levels_and_reports_bad_ones() {
        let mut app = app();
        assert_eq!(
            host_requests(&run(&mut app, "/reasoning high")),
            vec![HostRequest::SetReasoning(
                philo_agent_runtime::ReasoningEffort::High
            )]
        );
        let effects = run(&mut app, "/reasoning turbo");
        assert!(host_requests(&effects).is_empty());
        assert!(
            appended(&effects)[1]
                .text
                .starts_with("unknown reasoning level: turbo")
        );
        assert!(
            appended(&run(&mut app, "/reasoning"))[1]
                .text
                .starts_with("usage: /reasoning")
        );
    }

    #[test]
    fn image_registers_a_path_for_the_next_message() {
        let mut app = app();
        assert!(
            appended(&run(&mut app, "/image"))[1]
                .text
                .starts_with("usage: /image")
        );
        let lines = appended(&run(&mut app, "/image shots/a.png"));
        assert_eq!(
            lines[1].text,
            "image queued: shots/a.png (1 waiting for the next message)"
        );
        run(&mut app, "/image shots/b.png");
        assert_eq!(app.attachments().labels(), ["shots/a.png", "shots/b.png"]);
    }

    #[test]
    fn a_clipboard_image_joins_the_queue_and_rides_the_next_message() {
        let mut app = app();
        run(&mut app, "/image shots/a.png");
        let effects = app.attach_image("image/png".to_owned(), vec![0; 2048], "clipboard image");
        assert_eq!(
            texts(&appended(&effects)),
            ["attached: clipboard image (image/png, 2.0 KB) (2 waiting for the next message)"]
        );

        let effects = run(&mut app, "what is this?");
        assert_eq!(
            texts(&appended(&effects)),
            [
                "> what is this?",
                "> [attached shots/a.png]",
                "> [attached clipboard image (image/png, 2.0 KB)]",
            ]
        );
        let Effect::Submit { attachments, .. } = &effects[1] else {
            panic!("the message carries its attachments");
        };
        assert_eq!(attachments.len(), 2);
        assert!(app.attachments().is_empty(), "the queue drains on send");
    }

    #[test]
    fn a_refused_message_returns_to_the_input_with_its_survivors() {
        let mut app = app();
        run(&mut app, "/image missing.png");
        let effects = run(&mut app, "look");
        let Effect::Submit { text, attachments } = effects[1].clone() else {
            panic!("submit carries the draft");
        };
        // The driver could not read one of them and hands back the rest.
        app.restore_draft(&text, attachments[1..].to_vec());
        assert_eq!(app.input.text(), "look");
        assert!(app.attachments().is_empty());
    }

    #[test]
    fn ctrl_v_asks_the_driver_for_the_clipboard() {
        let mut app = app();
        assert_eq!(app.on_action(Action::Paste), vec![Effect::ReadClipboard]);
        let effects = app.clipboard_unavailable("clipboard is empty");
        assert_eq!(
            texts(&appended(&effects)),
            ["no image on the clipboard (clipboard is empty); attach a file with /image <path>"]
        );
        assert!(app.attachments().is_empty());
    }

    #[test]
    fn verbose_command_matches_the_toggle_chord() {
        let mut app = app();
        let lines = appended(&run(&mut app, "/verbose"));
        assert_eq!(lines[1].text, "info level: verbose");
        assert_eq!(app.level(), InfoLevel::Verbose);
        app.on_action(Action::ToggleLevel);
        assert_eq!(app.level(), InfoLevel::Default);
    }

    #[test]
    fn status_and_config_go_through_the_host() {
        let mut app = app();
        assert_eq!(
            host_requests(&run(&mut app, "/status")),
            vec![HostRequest::ShowStatus]
        );
        assert_eq!(
            host_requests(&run(&mut app, "/config")),
            vec![HostRequest::ShowConfig]
        );
    }

    #[test]
    fn quit_asks_once_while_a_turn_runs() {
        let mut app = app();
        app.set_busy(true, 0);
        let effects = run(&mut app, "/quit");
        assert!(!effects.contains(&Effect::Quit), "the first ask warns");
        assert!(appended(&effects)[1].text.contains("/quit again"));
        assert!(run(&mut app, "/quit").contains(&Effect::Quit));
    }

    #[test]
    fn quit_leaves_immediately_when_idle() {
        let mut app = app();
        assert!(run(&mut app, "/quit").contains(&Effect::Quit));
    }

    #[test]
    fn anything_between_two_quits_disarms_the_running_turn_exit() {
        let mut app = app();
        app.set_busy(true, 0);
        run(&mut app, "/quit");
        app.on_action(Action::InsertChar('x'));
        app.on_action(Action::Backspace);
        let effects = run(&mut app, "/quit");
        assert!(!effects.contains(&Effect::Quit), "it asks again");
    }

    #[test]
    fn tab_completes_a_unique_command_and_cycles_ambiguous_ones() {
        let mut app = app();
        type_text(&mut app, "/se");
        app.on_action(Action::Complete);
        assert_eq!(app.input.text(), "/sessions ");
        assert!(app.completion_line().is_none());

        app.input.clear();
        type_text(&mut app, "/s");
        app.on_action(Action::Complete);
        assert_eq!(app.input.text(), "/s", "the shared prefix is already typed");
        assert_eq!(
            app.completion_line(),
            Some("commands: sessions status".to_owned())
        );
        app.on_action(Action::Complete);
        assert_eq!(app.input.text(), "/sessions");
        assert_eq!(
            app.completion_line(),
            Some("commands: [sessions] status".to_owned())
        );
        app.on_action(Action::Complete);
        assert_eq!(app.input.text(), "/status");
        app.on_action(Action::Complete);
        assert_eq!(app.input.text(), "/sessions", "the cycle wraps");
    }

    #[test]
    fn tab_on_an_empty_slash_opens_the_whole_table() {
        let mut app = app();
        type_text(&mut app, "/");
        app.on_action(Action::Complete);
        crate::tests::assert_tui_snapshot!(
            "command_completion",
            app.completion_line().expect("menu is open")
        );
    }

    #[test]
    fn escape_closes_the_completion_menu_without_cancelling() {
        let mut app = app();
        app.set_busy(true, 0);
        type_text(&mut app, "/s");
        app.on_action(Action::Complete);
        assert!(app.on_action(Action::Escape).is_empty(), "no cancel");
        assert!(app.completion_line().is_none());
        assert_eq!(app.on_action(Action::Escape), vec![Effect::CancelActive]);
    }

    #[test]
    fn typing_closes_the_completion_menu() {
        let mut app = app();
        type_text(&mut app, "/s");
        app.on_action(Action::Complete);
        app.on_action(Action::InsertChar('t'));
        assert!(app.completion_line().is_none());
    }

    #[test]
    fn tab_without_a_slash_does_nothing() {
        let mut app = app();
        type_text(&mut app, "plain");
        app.on_action(Action::Complete);
        assert_eq!(app.input.text(), "plain");
        assert!(app.completion_line().is_none());
    }

    #[test]
    fn the_picker_moves_the_selection_and_loads_previews_lazily() {
        let mut app = app();
        app.open_picker(vec![SessionId::new("s-1"), SessionId::new("s-2")]);
        assert_eq!(app.claim_preview(), Some(SessionId::new("s-1")));

        let effects = app.on_action(Action::MoveDown);
        assert_eq!(
            host_requests(&effects),
            vec![HostRequest::LoadPreview(SessionId::new("s-2"))]
        );
        assert!(
            app.on_action(Action::MoveDown).is_empty(),
            "the last entry does not move"
        );
        let effects = app.on_action(Action::MoveUp);
        assert!(
            host_requests(&effects).is_empty(),
            "s-1 was already claimed"
        );
    }

    #[test]
    fn the_picker_switches_on_enter_and_closes_on_escape() {
        let mut app = app();
        app.open_picker(vec![SessionId::new("s-1"), SessionId::new("s-2")]);
        app.on_action(Action::MoveDown);
        assert_eq!(
            host_requests(&app.on_action(Action::Submit)),
            vec![HostRequest::SwitchSession(SessionId::new("s-2"))]
        );
        assert!(app.picker().is_none(), "Enter closes the overlay");

        app.open_picker(vec![SessionId::new("s-1")]);
        assert!(app.on_action(Action::Escape).is_empty());
        assert!(app.picker().is_none());
    }

    #[test]
    fn the_picker_refuses_to_switch_while_a_turn_runs() {
        let mut app = app();
        app.set_busy(true, 0);
        app.open_picker(vec![SessionId::new("s-1")]);
        let effects = app.on_action(Action::Submit);
        assert!(host_requests(&effects).is_empty());
        assert_eq!(appended(&effects)[0].kind, LineKind::Error);
        assert!(app.picker().is_some(), "the overlay stays open");
    }

    #[test]
    fn the_picker_does_not_type_into_the_input() {
        let mut app = app();
        app.open_picker(vec![SessionId::new("s-1")]);
        app.on_action(Action::InsertChar('x'));
        app.on_paste("pasted");
        assert!(app.input.is_empty());
    }

    #[test]
    fn approval_answers_are_binary_and_echoed() {
        let mut app = app();
        app.sync_confirmation(Some((ConfirmationId::for_test(1), request("run_command"))));
        let effects = app.on_action(Action::InsertChar('y'));
        assert_eq!(appended(&effects)[0].text, "allowed: run_command");
        assert_eq!(
            host_requests(&effects),
            vec![HostRequest::Respond(
                ConfirmationId::for_test(1),
                ConfirmationResponse::Allow
            )]
        );
        assert!(app.confirm_prompt().is_none());

        for (index, action) in [Action::InsertChar('n'), Action::Escape, Action::CtrlC]
            .into_iter()
            .enumerate()
        {
            let id = ConfirmationId::for_test(index as u64 + 2);
            app.sync_confirmation(Some((id, request("write_file"))));
            let effects = app.on_action(action);
            assert_eq!(appended(&effects)[0].text, "denied: write_file");
            assert_eq!(
                host_requests(&effects),
                vec![HostRequest::Respond(id, ConfirmationResponse::Deny)]
            );
        }
    }

    #[test]
    fn an_auto_denied_request_closes_the_overlay() {
        let mut app = app();
        app.sync_confirmation(Some((ConfirmationId::for_test(1), request("run_command"))));
        assert!(app.overlay_frame(4).is_some());
        // The channel denied everything when the operation settled.
        app.sync_confirmation(None);
        assert!(app.confirm_prompt().is_none());
        assert!(app.overlay_frame(4).is_none());
    }

    #[test]
    fn the_approval_overlay_wins_over_the_picker() {
        let mut app = app();
        app.open_picker(vec![SessionId::new("s-1")]);
        app.sync_confirmation(Some((ConfirmationId::for_test(7), request("run_command"))));
        let frame = app.overlay_frame(4).expect("an overlay is painted");
        assert!(frame.title.starts_with("approval required"));
        // Answering restores the picker underneath.
        app.on_action(Action::InsertChar('n'));
        let frame = app.overlay_frame(4).expect("the picker is still open");
        assert!(frame.title.starts_with("sessions"));
    }

    #[test]
    fn overlays_never_swallow_agent_events() {
        use philo_agent_runtime::{OperationId, OperationStatus, SettlementDurability};
        let mut app = app();
        app.open_picker(vec![SessionId::new("s-1")]);
        app.sync_confirmation(Some((ConfirmationId::for_test(1), request("run_command"))));
        let effects = app.on_agent_event(&AgentEvent::OperationSettled {
            operation_id: OperationId::new("op-1"),
            status: OperationStatus::Succeeded,
            durability: SettlementDurability::Confirmed,
        });
        assert_eq!(texts(&appended(&effects)), ["done (succeeded)"]);
    }

    #[test]
    fn ctrl_c_clears_nonempty_input_first() {
        let mut app = app();
        type_text(&mut app, "draft");
        assert!(app.on_action(Action::CtrlC).is_empty());
        assert!(app.input.is_empty());
    }

    #[test]
    fn ctrl_c_cancels_while_busy() {
        let mut app = app();
        app.set_busy(true, 0);
        assert_eq!(app.on_action(Action::CtrlC), vec![Effect::CancelActive]);
    }

    #[test]
    fn ctrl_c_twice_quits_when_idle_and_empty() {
        let mut app = app();
        let first = app.on_action(Action::CtrlC);
        assert!(appended(&first)[0].text.contains("again to exit"));
        assert_eq!(app.on_action(Action::CtrlC), vec![Effect::Quit]);
    }

    #[test]
    fn any_other_key_disarms_the_exit() {
        let mut app = app();
        app.on_action(Action::CtrlC);
        app.on_action(Action::InsertChar('x'));
        app.on_action(Action::Backspace);
        let effects = app.on_action(Action::CtrlC);
        let Effect::Append(_) = &effects[0] else {
            panic!("re-armed, not quit");
        };
    }

    #[test]
    fn escape_cancels_only_while_busy() {
        let mut app = app();
        assert!(app.on_action(Action::Escape).is_empty());
        app.set_busy(true, 0);
        assert_eq!(app.on_action(Action::Escape), vec![Effect::CancelActive]);
    }

    #[test]
    fn ctrl_d_quits_only_on_empty_input() {
        let mut app = app();
        type_text(&mut app, "x");
        assert!(app.on_action(Action::CtrlD).is_empty());
        app.on_action(Action::Backspace);
        assert_eq!(app.on_action(Action::CtrlD), vec![Effect::Quit]);
    }

    #[test]
    fn input_history_recalls_previous_submissions() {
        let mut app = app();
        run(&mut app, "first");
        run(&mut app, "second");

        type_text(&mut app, "dra");
        app.on_action(Action::MoveUp);
        assert_eq!(app.input.text(), "second");
        app.on_action(Action::MoveUp);
        assert_eq!(app.input.text(), "first");
        app.on_action(Action::MoveDown);
        assert_eq!(app.input.text(), "second");
        app.on_action(Action::MoveDown);
        assert_eq!(app.input.text(), "dra", "the stash comes back");
    }

    #[test]
    fn multiline_draft_moves_within_lines_before_history() {
        let mut app = app();
        run(&mut app, "top");
        type_text(&mut app, "a");
        app.on_action(Action::InsertNewline);
        type_text(&mut app, "b");
        // Cursor on line 2: the first MoveUp moves within the draft.
        app.on_action(Action::MoveUp);
        assert_eq!(app.input.text(), "a\nb");
        // On line 1 now: the next MoveUp recalls history.
        app.on_action(Action::MoveUp);
        assert_eq!(app.input.text(), "top");
    }

    #[test]
    fn toggle_level_flips_and_reports() {
        let mut app = app();
        assert_eq!(app.level(), InfoLevel::Default);
        app.on_action(Action::ToggleLevel);
        assert_eq!(app.level(), InfoLevel::Verbose);
        assert_eq!(app.status.level, InfoLevel::Verbose);
    }

    #[test]
    fn usage_events_update_the_status_bar() {
        let mut app = app();
        let usage = philo_agent_runtime::TokenUsage {
            input_tokens: Some(5),
            output_tokens: Some(7),
            ..Default::default()
        };
        app.on_agent_event(&AgentEvent::ModelUsageUpdated {
            model_call_id: philo_agent_runtime::ModelCallId::new("m-1"),
            usage,
        });
        assert_eq!(app.status.usage, Some(usage));
    }
}
