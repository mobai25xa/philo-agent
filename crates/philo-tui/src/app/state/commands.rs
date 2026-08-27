//! Slash-command execution and the auto command menu.

use super::App;
use super::line;
use crate::app::attachment::PendingAttachment;
use crate::app::command::{self, Command};
use crate::app::effect::{Effect, HostRequest};
use crate::app::text;
use crate::app::transcript::{InfoLevel, LineKind, TranscriptLine};

use crate::app::overlay::PANEL_PAD;

/// One rendered menu row: the marker-plus-usage cell and the summary cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MenuRow {
    pub(crate) usage: String,
    pub(crate) summary: String,
}

/// The frame the renderer paints above the composer while the menu is open.
/// `width` is the panel's inner content width; the shell adds the two border
/// columns and anchors the rounded panel at the input band's left edge, so
/// the menu spans exactly the composer's width (v0.44 §4.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandMenuFrame {
    pub(crate) rows: Vec<MenuRow>,
    pub(crate) selected: usize,
    pub(crate) width: usize,
}

/// The auto command menu: live-filtered candidates plus the highlight
/// cursor. It opens by itself whenever the draft is a bare `/word` and
/// closes as soon as the draft stops being one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CommandMenu {
    candidates: Vec<&'static str>,
    selected: usize,
}

impl CommandMenu {
    fn new(candidates: Vec<&'static str>) -> Self {
        Self {
            candidates,
            selected: 0,
        }
    }

    /// Re-filters after an edit. The highlight resets only when the
    /// candidate list actually changed.
    fn update(&mut self, candidates: Vec<&'static str>) {
        if candidates != self.candidates {
            self.candidates = candidates;
            self.selected = 0;
        }
    }

    /// Moves the highlight; returns whether it actually moved.
    fn move_up(&mut self) -> bool {
        if self.selected == 0 {
            return false;
        }
        self.selected -= 1;
        true
    }

    /// Moves the highlight; returns whether it actually moved.
    fn move_down(&mut self) -> bool {
        if self.selected + 1 >= self.candidates.len() {
            return false;
        }
        self.selected += 1;
        true
    }

    /// The highlighted command name.
    fn selected_name(&self) -> &'static str {
        self.candidates[self.selected]
    }

    /// Projects at most `max_rows` rows onto a panel exactly `max_width`
    /// outer columns wide (the input band's width), windowed so the
    /// highlighted row stays visible. Each row is `{marker}{usage}  {summary}`;
    /// the usage column aligns to the widest visible candidate and long
    /// rows truncate into the fixed text zone.
    pub(super) fn frame(&self, max_width: usize, max_rows: usize) -> CommandMenuFrame {
        let rows = max_rows.max(1).min(self.candidates.len());
        let start = if self.selected >= rows {
            self.selected + 1 - rows
        } else {
            0
        };
        let visible: Vec<&'static str> = self.candidates[start..start + rows].to_vec();
        let usage_width = visible
            .iter()
            .map(|name| command::spec(name).usage.len())
            .max()
            .unwrap_or(0);
        // The menu no longer follows its widest row: it always spans the
        // input band's exact width (v0.44 §4.1); content fits into the
        // constant text zone.
        // Marker + gap + aligned usage column + gap before the summary.
        let row_width = |summary_width: usize| 2 + usage_width + 2 + summary_width;
        let inner = max_width.saturating_sub(2).max(3);
        let text_zone = inner - PANEL_PAD * 2;
        let summary_budget = text_zone.saturating_sub(row_width(0));
        let rows = (start..start + rows)
            .map(|index| {
                let spec = command::spec(self.candidates[index]);
                let marker = if index == self.selected { "▶" } else { " " };
                let usage = format!("{marker} {:<usage_width$}  ", spec.usage);
                MenuRow {
                    usage: text::truncate(&usage, text_zone),
                    summary: text::truncate(spec.summary, summary_budget),
                }
            })
            .collect();
        CommandMenuFrame {
            rows,
            selected: self.selected - start,
            width: inner,
        }
    }
}

impl App {
    /// Re-derives the menu from the current draft: open over a bare
    /// `/word`, re-filtered on every edit, closed otherwise.
    pub(super) fn sync_completion(&mut self) {
        let candidates: Vec<&'static str> = command::candidates(&self.input.text())
            .iter()
            .map(|spec| spec.name)
            .collect();
        if candidates.is_empty() {
            self.completion = None;
        } else if let Some(menu) = &mut self.completion {
            menu.update(candidates);
        } else {
            self.completion = Some(CommandMenu::new(candidates));
        }
    }

    /// `Tab`: accepts the highlighted command while the menu is open;
    /// otherwise (re)opens the menu without changing the draft.
    pub(super) fn complete(&mut self) -> Vec<Effect> {
        if self.completion.is_some() {
            return self.accept_completion();
        }
        self.sync_completion();
        vec![]
    }

    /// Tab on an open menu: fills `/name ` and closes the menu.
    pub(super) fn accept_completion(&mut self) -> Vec<Effect> {
        let Some(name) = self.selected_command() else {
            return vec![];
        };
        self.bump_draft_generation();
        self.input.set_text(&format!("/{name} "));
        self.completion = None;
        vec![]
    }

    /// Up/Down on an open menu: moves the highlight.
    pub(super) fn move_completion(&mut self, up: bool) -> Vec<Effect> {
        if let Some(menu) = &mut self.completion {
            if up {
                menu.move_up();
            } else {
                menu.move_down();
            }
        }
        vec![]
    }

    /// Enter on an open menu: runs the highlighted command exactly as if
    /// it had been typed and submitted.
    pub(super) fn execute_completion(&mut self) -> Vec<Effect> {
        let Some(name) = self.selected_command() else {
            return vec![];
        };
        let text = format!("/{name}");
        let _ = self.input.take_text();
        self.bump_draft_generation();
        self.completion = None;
        self.history.push(text.clone());
        self.run_command(&text)
    }

    fn selected_command(&self) -> Option<&'static str> {
        self.completion.as_ref().map(CommandMenu::selected_name)
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
                    self.session_load_intent = Some(super::overlays::SessionLoadIntent::New);
                    effects.push(Effect::Host(HostRequest::NewSession));
                }
            }
            Ok(Command::Sessions) => effects.push(Effect::Host(HostRequest::OpenSessions)),
            Ok(Command::Rename { title: None }) => {
                lines.push(line(LineKind::Error, "usage: /rename <title>"));
            }
            Ok(Command::Rename { title: Some(title) }) => {
                if self.status.session.is_empty() {
                    lines.push(line(
                        LineKind::Error,
                        "error: no active session to rename yet",
                    ));
                } else {
                    effects.push(Effect::Host(HostRequest::RenameSession { title }));
                    lines.push(line(LineKind::Meta, "renaming the current session..."));
                }
            }
            Ok(Command::Models) => {
                self.expect_models_picker = true;
                effects.push(Effect::Host(HostRequest::OpenModels))
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
                    self.run_state.start_manual_compaction();
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
            Ok(Command::Theme { argument }) => lines.push(run_theme_command(argument)),
            Ok(Command::Config) => {
                self.expect_config_listing = true;
                effects.push(Effect::Host(HostRequest::ShowConfig));
            }
            Ok(Command::Quit) => {
                if self.has_activity() && !quit_armed {
                    self.quit_armed = true;
                    lines.push(line(
                        LineKind::Notice,
                        "the agent is still active: press Esc to cancel it, or /quit again to \
                         leave anyway",
                    ));
                } else if self.has_activity() {
                    effects.push(Effect::RequestShutdown);
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

/// `/theme`: bare shows the current scheme; a preset word pins that tier;
/// `sat N` / `light N` move the continuous sliders (new-color.md §四).
/// Presentation-only — the palette lives behind the render layer, so no
/// transcript state or host round-trip is involved.
fn run_theme_command(argument: Option<String>) -> TranscriptLine {
    use crate::render::theme::{self, ThemePreset, TuneAxis};

    let Some(argument) = argument.as_deref().map(str::trim).filter(|a| !a.is_empty()) else {
        return line(LineKind::Meta, theme::current_description());
    };
    if let Some(preset) = ThemePreset::from_name(argument) {
        return line(LineKind::Meta, theme::apply_preset(preset));
    }
    let outcome = match argument.split_once(' ') {
        Some(("sat", value)) => value
            .trim()
            .parse::<i32>()
            .map_err(|_| format!("usage: /theme sat 40..115 (got {value:?})"))
            .and_then(|value| theme::apply_tune(TuneAxis::Saturation, value)),
        Some(("light", value)) => value
            .trim()
            .parse::<i32>()
            .map_err(|_| format!("usage: /theme light -15..15 (got {value:?})"))
            .and_then(|value| theme::apply_tune(TuneAxis::Lightness, value)),
        Some(("bold", value)) => value
            .trim()
            .parse::<i32>()
            .map_err(|_| format!("usage: /theme bold 0..60 (got {value:?})"))
            .and_then(|value| theme::apply_tune(TuneAxis::BoldGain, value)),
        _ => Err(format!(
            "unknown theme: {argument} (try original|recommended|comfort|sat N|light N|bold N)"
        )),
    };
    match outcome {
        Ok(status) => line(LineKind::Meta, status),
        Err(message) => line(LineKind::Error, format!("error: {message}")),
    }
}
