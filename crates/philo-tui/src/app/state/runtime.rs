//! Agent events, compaction, config reload, and busy/exit chords.

use philo_agent_runtime::{AgentAvailability, AgentEvent, CompactionError, CompactionReport};

use super::App;
use super::line;
use crate::app::effect::Effect;
use crate::app::transcript::{InfoLevel, LineKind};

impl App {
    /// The event loop keeps the busy flag current (handles outstanding).
    pub fn set_busy(&mut self, busy: bool, queued: usize) {
        self.status.busy = busy;
        self.status.queued = queued;
        if !busy {
            self.automatic_compacting = false;
            self.sync_compacting_status();
            if !self.manual_compacting {
                self.activity.clear();
            }
        } else if !self.activity.is_active() {
            self.activity.wait_for_model();
        }
    }

    /// Whether presentation currently has a time-based animation.
    pub(crate) fn animation_active(&self) -> bool {
        self.activity.is_active()
    }

    /// Advances the manual/automatic compaction spinner. The driver owns
    /// invalidation; a tick never requests a terminal clear.
    pub(crate) fn on_tick(&mut self) -> bool {
        if !self.animation_active() {
            return false;
        }
        self.status.advance_spinner();
        self.activity.advance_spinner();
        true
    }

    /// Applies the terminal result of the driver's manual compaction future.
    pub(crate) fn finish_manual_compaction(
        &mut self,
        result: Result<CompactionReport, CompactionError>,
    ) -> Vec<Effect> {
        self.manual_compacting = false;
        self.activity.finish_manual_compaction();
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
        self.ingest_appends(vec![Effect::Append(vec![line])])
    }

    /// Starts rendering a different session: fresh transcript, sealed and
    /// unsealed cells, and usage. Native scrollback is no longer the history store.
    pub(crate) fn begin_session(&mut self, session_id: &str) {
        self.status.session = session_id.to_owned();
        self.status.usage = None;
        self.transcript = crate::app::transcript::Transcript::new(self.show_reasoning);
        self.activity.clear();
        self.cells.clear();
        self.scroll = crate::app::cells::ScrollState::follow();
        self.clear_selection();
    }

    /// Projects one agent event into transcript lines and status updates.
    /// Overlays never intercept this path: terminal events must render.
    pub fn on_agent_event(&mut self, event: &AgentEvent) -> Vec<Effect> {
        self.activity.on_event(event);
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
        let effects = if lines.is_empty() {
            vec![]
        } else {
            vec![Effect::Append(lines)]
        };
        let effects = self.ingest_appends(effects);
        self.sync_unsealed();
        effects
    }

    pub(super) fn apply_config_notice(
        &mut self,
        notice: crate::api::types::ConfigReloadNotice,
    ) -> Vec<Effect> {
        use crate::api::types::ConfigReloadNotice;
        match notice {
            ConfigReloadNotice::Applied {
                show_reasoning,
                verbose,
                context_window,
                model_name,
                runtime_pending,
                warnings,
            } => {
                self.show_reasoning = show_reasoning;
                self.transcript.set_show_reasoning(show_reasoning);
                self.status.context_window = context_window;
                self.status.model = model_name;
                self.level = if verbose {
                    InfoLevel::Verbose
                } else {
                    InfoLevel::Default
                };
                self.status.level = self.level;
                let mut lines = Vec::new();
                if runtime_pending {
                    let first = !self.status.config_reload_pending;
                    self.status.config_reload_pending = true;
                    if first {
                        lines.push(line(LineKind::Notice, "config: will apply after idle"));
                    }
                } else {
                    self.status.config_reload_pending = false;
                    lines.push(line(LineKind::Meta, "config reloaded"));
                }
                lines.extend(
                    warnings
                        .into_iter()
                        .map(|warning| line(LineKind::Notice, format!("warning: {warning}"))),
                );
                vec![Effect::Append(lines)]
            }
            ConfigReloadNotice::Failed {
                message,
                clear_pending,
            } => {
                if clear_pending {
                    self.status.config_reload_pending = false;
                }
                vec![Effect::Append(vec![line(
                    LineKind::Error,
                    format!("warning: {message}"),
                )])]
            }
            ConfigReloadNotice::Pending => {
                let first = !self.status.config_reload_pending;
                self.status.config_reload_pending = true;
                if first {
                    vec![Effect::Append(vec![line(
                        LineKind::Notice,
                        "config: will apply after idle",
                    )])]
                } else {
                    vec![]
                }
            }
        }
    }

    pub(super) fn escape(&mut self) -> Vec<Effect> {
        self.clear_selection();
        if self.manual_compacting {
            self.cancel_manual_compaction()
        } else if self.status.busy {
            vec![Effect::CancelActive]
        } else {
            vec![]
        }
    }

    pub(super) fn ctrl_c(&mut self) -> Vec<Effect> {
        if let Some(effect) = self.copy_selection() {
            self.exit_armed = false;
            return vec![effect];
        }
        if !self.input.is_empty() {
            self.bump_draft_generation();
            self.input.clear();
            self.history.reset_browse();
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

    pub(crate) fn sync_compacting_status(&mut self) {
        self.status
            .set_compacting(self.manual_compacting || self.automatic_compacting);
    }

    fn cancel_manual_compaction(&mut self) -> Vec<Effect> {
        self.manual_compacting = false;
        self.activity.finish_manual_compaction();
        self.sync_compacting_status();
        vec![
            Effect::CancelCompaction,
            Effect::Append(vec![line(LineKind::Notice, "context compaction cancelled")]),
        ]
    }
}
