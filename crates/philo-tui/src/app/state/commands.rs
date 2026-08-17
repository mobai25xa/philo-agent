//! Slash-command execution and Tab completion.

use super::App;
use super::line;
use crate::app::attachment::PendingAttachment;
use crate::app::command::{self, Command};
use crate::app::effect::{Effect, HostRequest};
use crate::app::transcript::{InfoLevel, LineKind, TranscriptLine};

/// An open command-completion menu: the candidates and the cycling cursor
/// (`None` while the input still holds the shared prefix).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CompletionMenu {
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

impl App {
    /// Tab: completes the command word. A single candidate completes
    /// outright; several open the menu at their shared prefix and each
    /// further Tab cycles through them.
    pub(super) fn complete(&mut self) -> Vec<Effect> {
        if let Some(menu) = self.completion.as_mut() {
            let name = menu.cycle();
            self.bump_draft_generation();
            self.input.set_text(&format!("/{name}"));
            return vec![];
        }
        let candidates: Vec<&'static str> = command::candidates(&self.input.text())
            .iter()
            .map(|spec| spec.name)
            .collect();
        match candidates.len() {
            0 => {}
            1 => {
                self.bump_draft_generation();
                self.input.set_text(&format!("/{} ", candidates[0]));
            }
            _ => {
                self.bump_draft_generation();
                self.input
                    .set_text(&format!("/{}", command::common_prefix(&candidates)));
                self.completion = Some(CompletionMenu::new(candidates));
            }
        }
        vec![]
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn run_command(&mut self, text: &str) -> Vec<Effect> {
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
                    self.activity.start_manual_compaction();
                    self.sync_compacting_status();
                    effects.push(Effect::StartCompaction);
                }
            }
            Ok(Command::Image { path: None }) => {
                lines.push(line(LineKind::Error, "usage: /image <path>"));
            }
            Ok(Command::Image { path: Some(path) }) => {
                self.bump_draft_generation();
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

    pub(super) fn toggle_level(&mut self) -> TranscriptLine {
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
    pub(crate) fn disarm_quit_unless_typing_quit(&mut self) {
        if self.quit_armed && !"/quit".starts_with(&self.input.text()) {
            self.quit_armed = false;
        }
    }
}
