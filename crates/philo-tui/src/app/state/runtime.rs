//! Frontend updates, operation events, compaction, and busy/exit chords.

use philo_agent_service::{
    DurableSessionView, FrontendAvailability, FrontendConfigEntry, FrontendGeneration,
    FrontendMaintenance, FrontendMaintenancePhase, FrontendOperationEvent, FrontendSnapshot,
    FrontendUpdate, FrontendUpdateKind as Kind, LiveOperationSnapshot, ServiceHealth,
};

use super::App;
use super::line;
use super::overlays::SessionLoadIntent;
use crate::app::effect::{Effect, HostRequest};
use crate::app::listings;
use crate::app::overlay::Preview;
use crate::app::session;
use crate::app::transcript::{InfoLevel, LineKind};

/// Preview rows loaded per session in the picker (the overlay body height).
const PREVIEW_ROWS: usize = 5;

impl App {
    /// Presentation busy/queued flags. Production sets these only from
    /// `AvailabilityChanged`; tests may call this directly.
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

    /// Starts rendering a different session: fresh transcript, cells, and
    /// usage. Native scrollback is no longer the history store.
    pub(crate) fn begin_session(&mut self, session_id: &str) {
        self.status.session = session_id.to_owned();
        self.status.usage = None;
        self.transcript = crate::app::transcript::Transcript::new(self.show_reasoning);
        self.activity.clear();
        self.cells.clear();
        self.scroll = crate::app::cells::ScrollState::follow();
        self.clear_selection();
    }

    /// Marks the next `SessionLoaded` as an initial or switched session.
    pub(crate) fn expect_session_load(&mut self, intent: SessionLoadIntent) {
        self.session_load_intent = Some(intent);
    }

    /// Projects one mapped operation event into the transcript store.
    /// Overlays never intercept this path: terminal facts must render.
    pub fn on_operation_event(&mut self, event: &FrontendOperationEvent) -> Vec<Effect> {
        self.activity.on_event(event);
        match event {
            FrontendOperationEvent::ModelUsageUpdated { usage, .. } => {
                self.status.usage = Some(*usage);
            }
            FrontendOperationEvent::ContextCompactionStarted => {
                self.automatic_compacting = true;
                self.sync_compacting_status();
            }
            FrontendOperationEvent::ContextCompactionCompleted { .. } => {
                self.automatic_compacting = false;
                self.status.usage = None;
                self.sync_compacting_status();
            }
            FrontendOperationEvent::ContextCompactionFailed { .. } => {
                self.automatic_compacting = false;
                self.sync_compacting_status();
            }
            _ => {}
        }
        self.transcript.apply(&mut self.cells, event, self.level);
        vec![]
    }

    /// Applies one frontend update to the presentation projection.
    pub fn apply_update(&mut self, update: &FrontendUpdate) -> Vec<Effect> {
        match &update.kind {
            Kind::CommandAccepted => Vec::new(),
            Kind::CommandRejected { reason } => self.apply_command_rejected(reason),
            Kind::OperationAccepted { .. } => Vec::new(),
            Kind::OperationEvent(event) => self.on_operation_event(event),
            Kind::AvailabilityChanged {
                availability,
                queued,
            } => {
                self.apply_availability(availability, *queued);
                Vec::new()
            }
            Kind::MaintenanceChanged(maintenance) => self.apply_maintenance(maintenance),
            Kind::SessionLoaded { session_id, view } => self.apply_session_loaded(session_id, view),
            Kind::SessionPreviewed { session_id, view } => {
                self.set_preview(
                    session_id,
                    Preview::Ready(session::preview_lines(view, PREVIEW_ROWS)),
                );
                Vec::new()
            }
            Kind::SessionListLoaded { session_ids } => self.apply_session_list(session_ids),
            Kind::GenerationInstalled { display } => self.apply_generation_installed(display),
            Kind::GenerationInstallFailed { message, .. } => {
                self.pending_model_switch = false;
                self.ingest_appends(vec![Effect::Append(vec![line(
                    LineKind::Error,
                    format!(
                        "error: model not switched: {message}; still on {}",
                        self.status.model
                    ),
                )])])
            }
            Kind::ConfigChanged { entries } => self.apply_config_changed(entries),
            Kind::ConfirmationRequested {
                confirmation_id,
                title,
                body,
            } => {
                self.sync_confirmation(Some((*confirmation_id, title.clone(), body.clone())));
                Vec::new()
            }
            Kind::ConfirmationResolved {
                confirmation_id, ..
            } => {
                if self
                    .confirm
                    .as_ref()
                    .is_some_and(|prompt| prompt.id == *confirmation_id)
                {
                    self.confirm = None;
                }
                Vec::new()
            }
            Kind::SnapshotReady(snapshot) => self.apply_snapshot(snapshot),
            Kind::ResyncRequired { .. } => Vec::new(),
            Kind::ServiceHealthChanged { health } => self.apply_health(health),
            Kind::StatusReady(status) => self.ingest_appends(vec![Effect::Append(
                listings::status_lines(&self.status.line(), self.attachments().summary(), status),
            )]),
        }
    }

    fn apply_command_rejected(&mut self, reason: &str) -> Vec<Effect> {
        if self.pending_model_switch {
            self.pending_model_switch = false;
            return self.ingest_appends(vec![Effect::Append(vec![line(
                LineKind::Error,
                format!(
                    "error: model not switched: {reason}; still on {}",
                    self.status.model
                ),
            )])]);
        }
        if self.manual_compacting {
            self.clear_manual_compaction();
            return self.ingest_appends(vec![Effect::Append(vec![line(
                LineKind::Error,
                format!("error: context compaction was not started: {reason}"),
            )])]);
        }
        self.ingest_appends(vec![Effect::Append(vec![line(
            LineKind::Error,
            format!("error: {reason}"),
        )])])
    }

    fn apply_availability(&mut self, availability: &FrontendAvailability, queued: usize) {
        match availability {
            FrontendAvailability::Idle => {
                self.manual_compacting = false;
                self.automatic_compacting = false;
                self.set_busy(false, queued);
            }
            FrontendAvailability::Busy { .. } => {
                self.automatic_compacting = false;
                self.sync_compacting_status();
                self.set_busy(true, queued);
            }
            FrontendAvailability::Compacting { .. } => {
                self.status.busy = false;
                self.status.queued = queued;
                if !self.manual_compacting && !self.automatic_compacting {
                    self.manual_compacting = true;
                    self.activity.start_manual_compaction();
                }
                self.sync_compacting_status();
            }
        }
    }

    fn apply_maintenance(&mut self, maintenance: &FrontendMaintenance) -> Vec<Effect> {
        match maintenance.phase {
            FrontendMaintenancePhase::Accepted
            | FrontendMaintenancePhase::Started
            | FrontendMaintenancePhase::Progress => {
                if !self.manual_compacting {
                    self.manual_compacting = true;
                    self.activity.start_manual_compaction();
                    self.sync_compacting_status();
                }
                Vec::new()
            }
            FrontendMaintenancePhase::Settled => {
                self.finish_manual_compaction_message(maintenance.message.as_deref())
            }
            FrontendMaintenancePhase::Failed => {
                self.clear_manual_compaction();
                let detail = maintenance
                    .message
                    .as_deref()
                    .unwrap_or("context compaction failed");
                self.ingest_appends(vec![Effect::Append(vec![line(
                    LineKind::Error,
                    format!("error: context compaction failed: {detail}"),
                )])])
            }
            FrontendMaintenancePhase::Cancelled => {
                self.clear_manual_compaction();
                self.ingest_appends(vec![Effect::Append(vec![line(
                    LineKind::Notice,
                    "context compaction cancelled",
                )])])
            }
        }
    }

    fn apply_session_list(&mut self, session_ids: &[String]) -> Vec<Effect> {
        if session_ids.is_empty() {
            return self.ingest_appends(vec![Effect::Append(vec![line(
                LineKind::Notice,
                "no sessions recorded yet; this one starts with the first message",
            )])]);
        }
        self.open_picker(session_ids.to_vec());
        self.claim_preview()
            .map(|id| vec![Effect::Host(HostRequest::LoadPreview(id))])
            .unwrap_or_default()
    }

    fn apply_session_loaded(&mut self, session_id: &str, view: &DurableSessionView) -> Vec<Effect> {
        let intent = self
            .session_load_intent
            .take()
            .unwrap_or(SessionLoadIntent::Switch);
        self.begin_session(session_id);
        match intent {
            SessionLoadIntent::New => self.ingest_appends(vec![Effect::Append(vec![line(
                LineKind::Meta,
                format!("new session: {session_id}"),
            )])]),
            SessionLoadIntent::Switch => {
                let mut lines = vec![line(LineKind::Meta, format!("session {session_id}"))];
                let history = session::history_lines(view);
                if history.is_empty() {
                    lines.push(line(LineKind::Meta, "(no history yet)"));
                } else {
                    lines.extend(history);
                }
                self.ingest_appends(vec![Effect::Append(lines)])
            }
            SessionLoadIntent::Snapshot => {
                let history = session::history_lines(view);
                if !history.is_empty() {
                    self.cells.push_closed(history);
                }
                Vec::new()
            }
        }
    }

    fn apply_generation_installed(&mut self, display: &FrontendGeneration) -> Vec<Effect> {
        let previous = self.status.model.clone();
        self.status.model.clone_from(&display.model_name);
        self.pending_model_switch = false;
        let text = if previous != display.model_name {
            format!("model: {}", display.model_name)
        } else if let Some(effort) = display
            .reasoning_effort
            .as_deref()
            .filter(|effort| !effort.is_empty() && *effort != "default")
        {
            format!(
                "reasoning: {} (from the next turn on)",
                effort_label(effort)
            )
        } else {
            format!("model: {}", display.model_name)
        };
        self.ingest_appends(vec![Effect::Append(vec![line(LineKind::Meta, text)])])
    }

    fn apply_config_changed(&mut self, entries: &[FrontendConfigEntry]) -> Vec<Effect> {
        self.apply_ui_entries(entries);
        if self.expect_config_listing {
            self.expect_config_listing = false;
            self.ingest_appends(vec![Effect::Append(listings::config_lines(entries))])
        } else {
            self.ingest_appends(vec![Effect::Append(vec![line(
                LineKind::Meta,
                "config reloaded",
            )])])
        }
    }

    fn apply_ui_entries(&mut self, entries: &[FrontendConfigEntry]) {
        for entry in entries {
            match entry.key.as_str() {
                "ui.show_reasoning" | "show_reasoning" => {
                    if let Some(value) = parse_bool(&entry.value) {
                        self.show_reasoning = value;
                        self.transcript.set_show_reasoning(value);
                    }
                }
                "verbose" | "ui.verbose" => {
                    if let Some(verbose) = parse_bool(&entry.value) {
                        self.level = if verbose {
                            InfoLevel::Verbose
                        } else {
                            InfoLevel::Default
                        };
                        self.status.level = self.level;
                    }
                }
                "context_window" | "ui.context_window" => {
                    self.status.context_window = entry.value.parse().ok();
                }
                "model" => self.status.model.clone_from(&entry.value),
                _ => {}
            }
        }
    }

    fn apply_snapshot(&mut self, snapshot: &FrontendSnapshot) -> Vec<Effect> {
        self.session_load_intent = Some(SessionLoadIntent::Snapshot);
        if let Some(session_id) = &snapshot.current_session_id {
            self.begin_session(session_id);
            if let Some(view) = &snapshot.durable_session_view {
                let history = session::history_lines(view);
                if !history.is_empty() {
                    self.cells.push_closed(history);
                }
            }
        } else if let Some(view) = &snapshot.durable_session_view {
            self.begin_session(&view.session_id);
            let history = session::history_lines(view);
            if !history.is_empty() {
                self.cells.push_closed(history);
            }
        }
        self.apply_live(&snapshot.live);
        self.apply_availability(&snapshot.availability, snapshot.queued.len());
        if let Some(usage) = snapshot.usage {
            self.status.usage = Some(usage);
        }
        self.status
            .model
            .clone_from(&snapshot.generation.model_name);
        if let Some(front) = snapshot.pending_confirmations.first() {
            self.sync_confirmation(Some((
                front.confirmation_id,
                front.title.clone(),
                front.body.clone(),
            )));
        } else {
            self.sync_confirmation(None);
        }
        if let Some(maintenance) = &snapshot.maintenance {
            let _ = self.apply_maintenance(maintenance);
        }
        Vec::new()
    }

    fn apply_live(&mut self, live: &LiveOperationSnapshot) {
        if let Some(usage) = live.usage {
            self.status.usage = Some(usage);
        }
        if !live.reasoning.is_empty() {
            let _ = self.on_operation_event(&FrontendOperationEvent::ReasoningDelta {
                model_call_id: live.model_call_id.clone().unwrap_or_default(),
                text: live.reasoning.clone(),
            });
        }
        if !live.text.is_empty() {
            let _ = self.on_operation_event(&FrontendOperationEvent::TextDelta {
                delta: live.text.clone(),
            });
        }
    }

    fn apply_health(&mut self, health: &ServiceHealth) -> Vec<Effect> {
        match health {
            ServiceHealth::Ok => Vec::new(),
            ServiceHealth::Degraded { message } => {
                self.ingest_appends(vec![Effect::Append(vec![line(
                    LineKind::Notice,
                    format!("warning: {message}"),
                )])])
            }
            ServiceHealth::RuntimeEpochEnded { message } => {
                self.ingest_appends(vec![Effect::Append(vec![line(
                    LineKind::Error,
                    format!("error: {message}"),
                )])])
            }
            ServiceHealth::ShuttingDown => self.ingest_appends(vec![Effect::Append(vec![line(
                LineKind::Notice,
                "service is shutting down",
            )])]),
        }
    }

    fn finish_manual_compaction_message(&mut self, message: Option<&str>) -> Vec<Effect> {
        self.clear_manual_compaction();
        let text = match message {
            Some(message) if message.contains("NothingToCompact") => line(
                LineKind::Notice,
                "nothing to compact: no older completed turns are available",
            ),
            Some(message) => {
                if let Some(boundary) = parse_compacted_boundary(message) {
                    self.status.usage = None;
                    line(
                        LineKind::Meta,
                        format!("context compacted through {boundary}"),
                    )
                } else {
                    self.status.usage = None;
                    line(LineKind::Meta, format!("context compacted: {message}"))
                }
            }
            None => {
                self.status.usage = None;
                line(LineKind::Meta, "context compacted")
            }
        };
        self.ingest_appends(vec![Effect::Append(vec![text])])
    }

    fn clear_manual_compaction(&mut self) {
        self.manual_compacting = false;
        self.activity.finish_manual_compaction();
        self.sync_compacting_status();
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
            return vec![Effect::InterruptCancel];
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
        self.clear_manual_compaction();
        vec![
            Effect::CancelCompaction,
            Effect::Append(vec![line(LineKind::Notice, "context compaction cancelled")]),
        ]
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_compacted_boundary(message: &str) -> Option<&str> {
    let start = message.find("covers_up_to:")?;
    let rest = message[start + "covers_up_to:".len()..].trim();
    let rest = rest.trim_start_matches('"');
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn effort_label(effort: &str) -> String {
    use philo_agent_service::FrontendReasoningEffort;
    match effort {
        "Minimal" => crate::app::command::reasoning_name(FrontendReasoningEffort::Minimal),
        "Low" => crate::app::command::reasoning_name(FrontendReasoningEffort::Low),
        "Medium" => crate::app::command::reasoning_name(FrontendReasoningEffort::Medium),
        "High" => crate::app::command::reasoning_name(FrontendReasoningEffort::High),
        "VeryHigh" => crate::app::command::reasoning_name(FrontendReasoningEffort::VeryHigh),
        "Maximum" => crate::app::command::reasoning_name(FrontendReasoningEffort::Maximum),
        other => other,
    }
    .to_owned()
}
