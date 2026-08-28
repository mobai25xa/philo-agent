//! Frontend updates, operation events, compaction, and busy/exit chords.

use philo_agent_service::{
    DurableSessionView, FrontendAvailability, FrontendConfigEntry, FrontendContextMessage,
    FrontendGeneration, FrontendGenerationChoice, FrontendMaintenance, FrontendMaintenancePhase,
    FrontendModelListing, FrontendOperationEvent, FrontendSessionSummary, FrontendSnapshot,
    FrontendUpdate, FrontendUpdateKind as Kind, FrontendUserPart, LiveOperationSnapshot,
    ServiceHealth,
};

use super::App;
use super::line;
use super::overlays::SessionLoadIntent;
use crate::app::effect::{Effect, HostRequest};
use crate::app::listings;
use crate::app::live_tool::{self, LiveBatch, LiveSlot, SlotSettle};
use crate::app::overlay::{PickerEntry, Preview};
use crate::app::session;
use crate::app::submit::{CancelDispatchResult, PendingSubmission};
use crate::app::transcript::{InfoLevel, LineKind};

/// Preview rows loaded per session in the picker (the overlay body height).
const PREVIEW_ROWS: usize = 5;

impl App {
    /// Presentation busy flag. Production sets this only from
    /// `AvailabilityChanged`; tests may call this directly.
    pub fn set_busy(&mut self, busy: bool) {
        self.status.busy = busy;
        if !busy {
            self.automatic_compacting = false;
            self.sync_compacting_status();
            if !self.manual_compacting {
                self.run_state.clear();
            }
        } else {
            self.run_state.ensure_waiting();
        }
    }

    /// Whether presentation currently has a time-based animation: the
    /// run-state spinner, pending paced stream text, or the scrollbar's
    /// scroll-heat window (P2).
    pub(crate) fn animation_active(&self) -> bool {
        self.run_state.is_active()
            || !self.pacer.is_empty()
            || self
                .tool_batch
                .as_ref()
                .is_some_and(|batch| !batch.all_settled())
            || self.scrollbar_active()
    }

    /// Advances one animation tick: the spinner and the pacer's release of
    /// buffered stream characters. `dt` is the driver's animation cadence;
    /// a tick never requests a clear.
    pub(crate) fn on_tick(&mut self, dt: std::time::Duration) -> bool {
        let mut dirty = false;
        if self.run_state.is_active() {
            self.run_state.advance_spinner();
            dirty = true;
        }
        // Live tool cards tick their spinner + elapsed (v4.0 P3 §4.1).
        if self.tool_batch.is_some() {
            self.refresh_live_cards();
            dirty = true;
        }
        let pieces = self.pacer.drain(dt);
        dirty | self.write_paced(pieces)
    }

    /// Rewrites the in-flight live card cell with the current spinner frame
    /// and elapsed; settled batches are already final and skip the rewrite.
    fn refresh_live_cards(&mut self) {
        let Some(batch) = self.tool_batch.as_ref() else {
            return;
        };
        if batch.all_settled() {
            return;
        }
        let Some(cell_index) = batch.cell_index else {
            return;
        };
        let spinner = self.run_state.spinner_frame().to_owned();
        let cell = live_tool::live_cell(batch, &spinner);
        self.cells.replace_cell(cell_index, cell);
    }

    /// Starts rendering a different session: fresh transcript, cells, and
    /// usage. The usage cache restores the last-seen telemetry for sessions
    /// already visited in this process; new sessions start at `-`.
    pub(crate) fn begin_session(&mut self, session_id: &str) {
        self.status.session = session_id.to_owned();
        self.status.usage = self.usage_cache.get(session_id).copied();
        // Per-session model/effort restore: hot sessions already in the cache
        // refresh the top-right corner; cold sessions fall back to whatever
        // `apply_session_loaded`/`apply_snapshot` set last.
        if let Some(choice) = self.model_cache.get(session_id) {
            self.status.model = choice.model_name.clone();
            self.status.effort = choice.reasoning_effort.clone();
            self.status.provider = choice.provider.clone();
        }
        self.transcript = crate::app::transcript::Transcript::new(self.show_reasoning);
        self.run_state.clear();
        self.clear_stream();
        self.cells.clear();
        self.reasoning_manually_expanded.clear();
        self.tool_batch = None;
        self.tool_cards_expanded.clear();
        self.tool_cards_folded.clear();
        self.scroll = crate::app::cells::ScrollState::follow();
        self.clear_selection();
    }

    /// Marks the next `SessionLoaded` as an initial or switched session.
    pub(crate) fn expect_session_load(&mut self, intent: SessionLoadIntent) {
        self.session_load_intent = Some(intent);
    }

    /// Projects one mapped operation event into the transcript store.
    /// Overlays never intercept this path: terminal facts must render.
    ///
    /// v2.2: streaming deltas detour through the pacer; every other event
    /// flushes the backlog first, so structural boundaries observe the full
    /// text — cancellation reveals it instantly and settlement dedups the
    /// completed-message echo exactly like an unpaced run.
    pub fn on_operation_event(&mut self, event: &FrontendOperationEvent) -> Vec<Effect> {
        // Peek the turn clock before terminal events stop it, so the
        // settlement line can cite the duration (design §2.4).
        let turn_elapsed = terminal_event_elapsed(&self.run_state, event);
        self.run_state.on_event(event);
        match event {
            FrontendOperationEvent::ModelUsageUpdated { usage, .. } => {
                self.status.usage = Some(*usage);
                self.usage_cache.insert(self.status.session.clone(), *usage);
            }
            FrontendOperationEvent::ContextCompactionStarted => {
                self.automatic_compacting = true;
                self.sync_compacting_status();
            }
            FrontendOperationEvent::ContextCompactionCompleted { .. } => {
                self.automatic_compacting = false;
                self.clear_usage();
                self.sync_compacting_status();
            }
            FrontendOperationEvent::ContextCompactionFailed { .. } => {
                self.automatic_compacting = false;
                self.sync_compacting_status();
            }
            _ => {}
        }
        match event {
            FrontendOperationEvent::TextDelta { delta } => {
                self.pace_delta(crate::app::transcript::LineKind::Answer, delta);
                return vec![];
            }
            // Reasoning dropped by `[ui].show_reasoning=false` never paces.
            FrontendOperationEvent::ReasoningDelta { .. } if !self.show_reasoning => {}
            FrontendOperationEvent::ReasoningDelta { text, .. } => {
                self.pace_delta(crate::app::transcript::LineKind::Reasoning, text);
                return vec![];
            }
            // v4.0 P3: default-mode tool events drive the live cards directly
            // (a running card rewrites in place, progress bounds its output).
            // Verbose mode keeps the transcript's older line-based card.
            FrontendOperationEvent::ToolBatchRequested { .. }
            | FrontendOperationEvent::ToolExecutionStarted { .. }
            | FrontendOperationEvent::ToolExecutionProgress { .. }
            | FrontendOperationEvent::ToolExecutionCompleted { .. }
                if self.level != InfoLevel::Verbose =>
            {
                self.flush_stream();
                self.apply_tool_event(event);
                return vec![];
            }
            // Cancellation settles any live tool cards to `✗ cancelled`.
            FrontendOperationEvent::CancellationRequested { .. }
            | FrontendOperationEvent::TurnCancelled { .. } => self.settle_live_tools_cancelled(),
            // Terminal events drop the batch so later ticks never rewrite.
            FrontendOperationEvent::OperationSettled { .. }
            | FrontendOperationEvent::TurnFailed { .. } => self.tool_batch = None,
            _ => {}
        }
        self.flush_stream();
        self.transcript
            .apply(&mut self.cells, event, self.level, turn_elapsed);
        vec![]
    }

    /// Dispatches one default-mode tool event to the live card machine.
    fn apply_tool_event(&mut self, event: &FrontendOperationEvent) {
        match event {
            FrontendOperationEvent::ToolBatchRequested { call_count, .. } => {
                self.begin_tool_batch(*call_count);
            }
            FrontendOperationEvent::ToolExecutionStarted {
                index,
                tool_name,
                arguments,
                ..
            } => {
                self.start_live_tool(*index, tool_name, arguments);
            }
            FrontendOperationEvent::ToolExecutionProgress { index, tail, .. } => {
                self.progress_live_tool(*index, tail);
            }
            FrontendOperationEvent::ToolExecutionCompleted {
                index,
                tool_name,
                result,
                display,
                ..
            } => {
                self.complete_live_tool(*index, tool_name, result, display.as_ref());
            }
            _ => {}
        }
    }

    /// A batch announcement opens the live batch; multi-call batches create
    /// their tree cell immediately (the parent header carries the count).
    fn begin_tool_batch(&mut self, call_count: usize) {
        self.cells.close_open();
        self.cells.seal_think();
        self.tool_batch = Some(LiveBatch::new(call_count));
        if call_count > 1 {
            let spinner = self.run_state.spinner_frame().to_owned();
            let batch = self.tool_batch.as_ref().expect("batch just created");
            let cell = live_tool::live_cell(batch, &spinner);
            self.cells.push_closed([cell]);
            let index = self.cells.display_len() - 1;
            self.tool_batch.as_mut().expect("batch just created").cell_index = Some(index);
        }
    }

    /// A tool started: record its slot; the single tool's running card is
    /// created here (the tree cell already exists).
    fn start_live_tool(&mut self, index: usize, tool_name: &str, arguments: &str) {
        if self.tool_batch.is_none() {
            self.begin_tool_batch(1);
        }
        let Some(batch) = self.tool_batch.as_mut() else {
            return;
        };
        if batch.slot(index).is_some() {
            return;
        }
        let slot = LiveSlot {
            index,
            tool_name: tool_name.to_owned(),
            arguments: arguments.to_owned(),
            started_at: std::time::Instant::now(),
            output: String::new(),
            truncated: false,
            settled: None,
        };
        batch.slots.push(slot);
        if batch.total == 1 {
            let (spinner, elapsed) = {
                let batch = self.tool_batch.as_ref().expect("batch present");
                (
                    self.run_state.spinner_frame().to_owned(),
                    batch.elapsed(),
                )
            };
            let (tool_name, arguments) = {
                let slot = self
                    .tool_batch
                    .as_ref()
                    .expect("batch present")
                    .slot(index)
                    .expect("slot just pushed");
                (slot.tool_name.clone(), slot.arguments.clone())
            };
            let cell = crate::app::tool_card::running_cell(
                &tool_name,
                &arguments,
                "",
                false,
                &spinner,
                elapsed,
            );
            self.cells.push_closed([cell]);
            let cell_index = self.cells.display_len() - 1;
            self.tool_batch
                .as_mut()
                .expect("batch present")
                .cell_index = Some(cell_index);
        } else {
            self.rewrite_tree();
        }
    }

    /// Progress appends to the slot's bounded output (single cards only;
    /// tree children are headers) and rewrites the live cell.
    fn progress_live_tool(&mut self, index: usize, tail: &str) {
        let Some(batch) = self.tool_batch.as_mut() else {
            return;
        };
        let Some(slot) = batch.slot_mut(index) else {
            return;
        };
        if slot.settled.is_some() {
            return;
        }
        let remaining = crate::app::tool_card::LIVE_TEXT_CHARS_MAX.saturating_sub(slot.output.len());
        if remaining == 0 {
            slot.truncated = true;
            return;
        }
        let take = tail.len().min(remaining);
        slot.output.push_str(&tail[..take]);
        if take < tail.len() {
            slot.truncated = true;
        }
        if batch.total == 1 {
            let cell_index = batch.cell_index;
            if let Some(cell_index) = cell_index {
                let (spinner, elapsed, tool_name, arguments, output, truncated) = {
                    let batch = self.tool_batch.as_ref().expect("batch present");
                    let slot = batch.slot(index).expect("slot present");
                    (
                        self.run_state.spinner_frame().to_owned(),
                        batch.slot_elapsed(slot),
                        slot.tool_name.clone(),
                        slot.arguments.clone(),
                        slot.output.clone(),
                        slot.truncated,
                    )
                };
                let cell = crate::app::tool_card::running_cell(
                    &tool_name, &arguments, &output, truncated, &spinner, elapsed,
                );
                self.cells.replace_cell(cell_index, cell);
            }
        }
    }

    /// A tool completed: settle the slot in place — the single card rewrites
    /// its cell with the settled card, the tree cell folds the child in.
    fn complete_live_tool(
        &mut self,
        index: usize,
        tool_name: &str,
        result: &philo_agent_service::FrontendToolResult,
        display: Option<&philo_agent_service::FrontendToolDisplay>,
    ) {
        let Some(batch) = self.tool_batch.as_ref() else {
            self.cells
                .push_closed(crate::app::tool_card::default_card(
                    tool_name,
                    "",
                    result,
                    display,
                    None,
                ));
            return;
        };
        let single = batch.total == 1;
        let cell_index = batch.cell_index;
        let elapsed = batch.slot(index).map(|slot| batch.slot_elapsed(slot));
        let tool_name = batch
            .slot(index)
            .map(|slot| slot.tool_name.clone())
            .unwrap_or_else(|| tool_name.to_owned());
        let arguments = batch
            .slot(index)
            .map(|slot| slot.arguments.clone())
            .unwrap_or_default();
        let Some(batch) = self.tool_batch.as_mut() else {
            return;
        };
        let Some(slot) = batch.slot_mut(index) else {
            return;
        };
        if slot.settled.is_some() {
            return;
        }
        slot.settled = Some(SlotSettle {
            result: result.clone(),
            display: display.cloned(),
            cancelled: false,
        });
        if single {
            if let Some(cell_index) = cell_index {
                let settled = crate::app::tool_card::default_card(
                    &tool_name,
                    &arguments,
                    result,
                    display,
                    elapsed,
                );
                self.cells.replace_tail(cell_index, settled);
            }
        } else {
            self.rewrite_tree();
        }
    }

    /// Cancellation rewrites every still-running slot as `✗ cancelled` — the
    /// highest-priority settle that later completions may not overwrite.
    fn settle_live_tools_cancelled(&mut self) {
        let Some(batch) = self.tool_batch.as_mut() else {
            return;
        };
        let single = batch.total == 1;
        let cell_index = batch.cell_index;
        for slot in &mut batch.slots {
            if slot.settled.is_none() {
                slot.settled = Some(SlotSettle {
                    result: philo_agent_service::FrontendToolResult::Error {
                        code: "cancelled".to_owned(),
                        message: String::new(),
                    },
                    display: None,
                    cancelled: true,
                });
            }
        }
        if single {
            if let Some(cell_index) = cell_index {
                let (tool_name, arguments) = {
                    let batch = self.tool_batch.as_ref().expect("batch present");
                    let slot = batch.slots.first().expect("single slot");
                    (slot.tool_name.clone(), slot.arguments.clone())
                };
                let cell =
                    crate::app::tool_card::cancelled_cell(&tool_name, &arguments);
                self.cells.replace_tail(cell_index, vec![cell]);
            }
        } else {
            self.rewrite_tree();
        }
    }

    /// Rebuilds the tree cell from the batch's current children.
    fn rewrite_tree(&mut self) {
        let Some(batch) = self.tool_batch.as_ref() else {
            return;
        };
        let Some(cell_index) = batch.cell_index else {
            return;
        };
        let spinner = self.run_state.spinner_frame().to_owned();
        let cell = live_tool::live_cell(batch, &spinner);
        self.cells.replace_cell(cell_index, cell);
    }

    /// Applies one frontend update to the presentation projection.
    pub fn apply_update(&mut self, update: &FrontendUpdate) -> Vec<Effect> {
        match &update.kind {
            Kind::CommandAccepted | Kind::CompactionAccepted { .. } => Vec::new(),
            Kind::SubmitAccepted { .. } => {
                // Commit path is driven by the driver with intent correlation.
                Vec::new()
            }
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
            Kind::SessionListLoaded { sessions } => self.apply_session_list(sessions),
            Kind::ModelListLoaded { models } => {
                let open_picker = std::mem::take(&mut self.expect_models_picker);
                self.apply_model_catalog(models, open_picker)
            }
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
            Kind::StatusReady(status) => {
                self.ingest_appends(vec![Effect::Append(listings::status_lines(
                    &self.status.summary_line(),
                    self.attachments().summary(),
                    status,
                ))])
            }
        }
    }

    fn apply_command_rejected(
        &mut self,
        reason: &philo_agent_service::CommandReject,
    ) -> Vec<Effect> {
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

    fn apply_availability(&mut self, availability: &FrontendAvailability, _queued: usize) {
        match availability {
            FrontendAvailability::Idle => {
                self.manual_compacting = false;
                self.automatic_compacting = false;
                self.set_busy(false);
            }
            FrontendAvailability::Busy { .. } => {
                self.automatic_compacting = false;
                self.sync_compacting_status();
                self.set_busy(true);
            }
            FrontendAvailability::Compacting { .. } => {
                self.status.busy = false;
                if !self.manual_compacting && !self.automatic_compacting {
                    self.manual_compacting = true;
                    self.run_state.start_manual_compaction();
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
                    self.run_state.start_manual_compaction();
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
                if !self.manual_compacting {
                    return Vec::new();
                }
                self.clear_manual_compaction();
                self.ingest_appends(vec![Effect::Append(vec![line(
                    LineKind::Meta,
                    "context compaction cancelled",
                )])])
            }
        }
    }

    fn apply_session_list(&mut self, summaries: &[FrontendSessionSummary]) -> Vec<Effect> {
        if summaries.is_empty() {
            return self.ingest_appends(vec![Effect::Append(vec![line(
                LineKind::Meta,
                "no sessions recorded yet; this one starts with the first message",
            )])]);
        }
        let now = unix_now();
        let entries = summaries
            .iter()
            .map(|summary| PickerEntry {
                id: summary.session_id.clone(),
                primary: summary
                    .title
                    .clone()
                    .unwrap_or_else(|| summary.session_id.clone()),
                secondary: summary
                    .updated_at
                    .and_then(|updated_at| relative_time(updated_at, now))
                    .unwrap_or_default(),
                group: String::new(),
                marked: summary.session_id == self.status.session,
                tiers: Vec::new(),
            })
            .collect();
        self.open_picker(entries);
        self.refresh_picker_preview()
    }

    /// Model-catalog load: tracks the dashboard's provider fact from the
    /// current entry, then optionally opens the `/models` picker. Catalog
    /// loads that were not user-opened stay silent.
    pub(crate) fn apply_model_catalog(
        &mut self,
        models: &[FrontendModelListing],
        open_picker: bool,
    ) -> Vec<Effect> {
        if let Some(current) = models.iter().find(|listing| listing.current) {
            self.status.provider = Some(current.provider.clone());
        }
        if !open_picker {
            return Vec::new();
        }
        if models.is_empty() {
            return self.ingest_appends(vec![Effect::Append(vec![line(
                LineKind::Meta,
                "no models configured; add [providers.<id>] sections to config.toml",
            )])]);
        }
        let entries = models
            .iter()
            .map(|listing| PickerEntry {
                id: listing.id.clone(),
                primary: listing.id.clone(),
                // The active model is flagged with the `current` word rather
                // than the session marker dot (contract §4.2).
                secondary: if listing.current { "current" } else { "" }.to_owned(),
                group: listing.provider.clone(),
                marked: listing.current,
                tiers: listing.reasoning_tiers.clone(),
            })
            .collect();
        self.open_model_picker(entries);
        Vec::new()
    }

    fn apply_session_loaded(&mut self, session_id: &str, view: &DurableSessionView) -> Vec<Effect> {
        let intent = self
            .session_load_intent
            .take()
            .unwrap_or(SessionLoadIntent::Switch);
        self.begin_session(session_id);
        // Cross-process usage restore: the session store carries the last
        // settled turn's usage, so the telemetry reads the saved value
        // immediately on load, not `-`.
        if let Some(usage) = view.usage {
            self.status.usage = Some(usage);
            self.usage_cache.insert(session_id.to_owned(), usage);
        }
        // Cross-process model/effort restore: the durable session view
        // carries the last settled turn's generation choice, so the
        // top-right corner shows the saved model immediately on load.
        if let Some(choice) = &view.generation {
            self.status.model = choice.model_name.clone();
            self.status.effort = choice.reasoning_effort.clone();
            self.status.provider = choice.provider.clone();
            self.model_cache.insert(session_id.to_owned(), choice.clone());
        }
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
        self.status.effort = effort_value(display.reasoning_effort.as_deref());
        self.status.provider = display.provider.clone();
        self.pending_model_switch = false;
        // Record the installed model/effort for the current session so a
        // switch-back restores it without a store round-trip.
        if !self.status.session.is_empty() {
            self.model_cache.insert(
                self.status.session.clone(),
                FrontendGenerationChoice {
                    provider: display.provider.clone(),
                    model_name: display.model_name.clone(),
                    reasoning_effort: display.reasoning_effort.clone(),
                },
            );
        }
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
        // The catalog's current entry moved with the install; one refresh
        // keeps the dashboard's provider fact honest.
        let mut effects =
            self.ingest_appends(vec![Effect::Append(vec![line(LineKind::Meta, text)])]);
        effects.push(Effect::Host(HostRequest::RefreshModels));
        effects
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
                        if !value {
                            // Hiding reasoning seals live think timers; the
                            // backlog must not resurrect hidden text.
                            self.flush_stream();
                        }
                        self.show_reasoning = value;
                        self.transcript.set_show_reasoning(&mut self.cells, value);
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
                // Cross-process usage restore on resync.
                if let Some(usage) = view.usage {
                    self.status.usage = Some(usage);
                    self.usage_cache.insert(session_id.clone(), usage);
                }
                // Cross-process model/effort restore on resync.
                if let Some(choice) = &view.generation {
                    self.model_cache
                        .insert(session_id.clone(), choice.clone());
                }
                let history = session::history_lines(view);
                if !history.is_empty() {
                    self.cells.push_closed(history);
                }
            }
        } else if let Some(view) = &snapshot.durable_session_view {
            self.begin_session(&view.session_id);
            if let Some(usage) = view.usage {
                self.status.usage = Some(usage);
                self.usage_cache.insert(view.session_id.clone(), usage);
            }
            if let Some(choice) = &view.generation {
                self.model_cache
                    .insert(view.session_id.clone(), choice.clone());
            }
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
        self.status.effort = effort_value(snapshot.generation.reasoning_effort.as_deref());
        self.status.provider = snapshot.generation.provider.clone();
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
        let mut effects = Vec::new();
        if !matches!(snapshot.health, ServiceHealth::Ok) {
            effects.extend(self.apply_health(&snapshot.health));
        }
        let restored = self.reconcile_dispatching_after_snapshot(snapshot);
        effects.extend(self.ingest_appends(restored));
        effects
    }

    fn reconcile_dispatching_after_snapshot(&mut self, snapshot: &FrontendSnapshot) -> Vec<Effect> {
        let Some(pending) = self.submit_state.pending().cloned() else {
            return Vec::new();
        };
        if let Some(operation_id) = operation_for_pending_intent(snapshot, &pending) {
            return self.on_submit_accepted(pending.intent_id, operation_id);
        }
        self.restore_pending_after_interrupt(line(LineKind::Meta, "提交未确认，内容已恢复"))
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
                self.clear_runtime_presence();
                let notice = line(LineKind::Error, format!("error: {message}"));
                let restored = self.restore_pending_after_interrupt(notice.clone());
                if restored.is_empty() {
                    self.ingest_appends(vec![Effect::Append(vec![notice])])
                } else {
                    self.ingest_appends(restored)
                }
            }
            ServiceHealth::ShuttingDown => self.ingest_appends(vec![Effect::Append(vec![line(
                LineKind::Notice,
                "service is shutting down",
            )])]),
        }
    }

    fn clear_runtime_presence(&mut self) {
        self.manual_compacting = false;
        self.automatic_compacting = false;
        self.run_state.clear();
        // A runtime epoch end orphans any buffered text; drop it with the
        // presence it belonged to.
        self.clear_stream();
        self.set_busy(false);
    }

    fn finish_manual_compaction_message(&mut self, message: Option<&str>) -> Vec<Effect> {
        self.clear_manual_compaction();
        let text = match message {
            Some(message) if message.contains("NothingToCompact") => line(
                LineKind::Meta,
                "nothing to compact: no older completed turns are available",
            ),
            Some(message) => {
                if let Some(boundary) = parse_compacted_boundary(message) {
                    self.clear_usage();
                    line(
                        LineKind::Meta,
                        format!("context compacted through {boundary}"),
                    )
                } else {
                    self.clear_usage();
                    line(LineKind::Meta, format!("context compacted: {message}"))
                }
            }
            None => {
                self.clear_usage();
                line(LineKind::Meta, "context compacted")
            }
        };
        self.ingest_appends(vec![Effect::Append(vec![text])])
    }

    /// Drops the live usage and its cached copy for the current session.
    /// Compaction rewrites the context window, so the prior token counts no
    /// longer apply; the cache is cleared so a switch-back does not resurrect
    /// a stale telemetry value.
    fn clear_usage(&mut self) {
        self.status.usage = None;
        self.usage_cache.remove(&self.status.session);
    }

    fn clear_manual_compaction(&mut self) {
        self.manual_compacting = false;
        self.run_state.finish_manual_compaction();
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
            self.sync_completion();
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
            LineKind::Meta,
            "press Ctrl+C again to exit",
        )])]
    }

    pub(crate) fn sync_compacting_status(&mut self) {
        self.status.compacting = self.manual_compacting || self.automatic_compacting;
    }

    fn cancel_manual_compaction(&mut self) -> Vec<Effect> {
        vec![Effect::CancelCompaction]
    }

    pub(super) fn on_compaction_cancel_dispatch_finished(
        &mut self,
        result: CancelDispatchResult,
    ) -> Vec<Effect> {
        match result {
            CancelDispatchResult::Enqueued(_) => {
                self.clear_manual_compaction();
                vec![Effect::Append(vec![line(
                    LineKind::Meta,
                    "context compaction cancelled",
                )])]
            }
            CancelDispatchResult::Backpressured => {
                vec![Effect::Append(vec![line(
                    LineKind::Notice,
                    "取消请求未发送",
                )])]
            }
            CancelDispatchResult::Disconnected { lane } => {
                vec![Effect::Append(vec![line(
                    LineKind::Error,
                    format!("error: frontend disconnected ({lane}); cancel not sent"),
                )])]
            }
        }
    }
}

fn snapshot_in_flight_ids(snapshot: &FrontendSnapshot) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(id) = &snapshot.live.operation_id {
        ids.push(id.clone());
    }
    for queued in &snapshot.queued {
        if !ids.iter().any(|id| id == &queued.operation_id) {
            ids.push(queued.operation_id.clone());
        }
    }
    if let FrontendAvailability::Busy { operation_id } = &snapshot.availability
        && !ids.iter().any(|id| id == operation_id)
    {
        ids.push(operation_id.clone());
    }
    ids
}

fn last_user_parts(snapshot: &FrontendSnapshot) -> Option<&[FrontendUserPart]> {
    snapshot
        .durable_session_view
        .as_ref()?
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            FrontendContextMessage::User { parts } => Some(parts.as_slice()),
            _ => None,
        })
}

/// When the pending draft already landed in the snapshot, the in-flight
/// live/queued id is this intent's operation.
fn operation_for_pending_intent(
    snapshot: &FrontendSnapshot,
    pending: &PendingSubmission,
) -> Option<String> {
    let ids = snapshot_in_flight_ids(snapshot);
    if ids.is_empty() {
        return None;
    }
    let parts = last_user_parts(snapshot)?;
    let draft_landed = !pending.draft.is_empty()
        && parts
            .iter()
            .any(|part| matches!(part, FrontendUserPart::Text(text) if text == &pending.draft));
    let attachments_landed = pending.draft.is_empty()
        && !pending.attachments.is_empty()
        && parts
            .iter()
            .any(|part| matches!(part, FrontendUserPart::Image { .. }));
    if draft_landed || attachments_landed {
        Some(ids[0].clone())
    } else {
        None
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Terminal events read the turn clock one last time before the run-state
/// machine stops it; every other event leaves the line out.
fn terminal_event_elapsed(
    run_state: &crate::app::run_state::RunState,
    event: &FrontendOperationEvent,
) -> Option<std::time::Duration> {
    if matches!(
        event,
        FrontendOperationEvent::OperationSettled { .. } | FrontendOperationEvent::TurnFailed { .. }
    ) {
        run_state.elapsed()
    } else {
        None
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// Compact relative age for picker meta columns: `now`, `12m`, `3h`, `5d`,
/// then a bare date. Presentation-only; unknown stamps render nothing.
fn relative_time(stamp: u64, now: u64) -> Option<String> {
    let age = now.checked_sub(stamp)?;
    Some(if age < 60 {
        "now".to_owned()
    } else if age < 3600 {
        format!("{}m", age / 60)
    } else if age < 86_400 {
        format!("{}h", age / 3600)
    } else if age < 7 * 86_400 {
        format!("{}d", age / 86_400)
    } else {
        // Best-effort civil date from the unix stamp (UTC, presentation only).
        let days = age / 86_400;
        let (year, month, day) = civil_from_days((stamp / 86_400) as i64);
        if year >= 1000 {
            format!("{year:04}-{month:02}-{day:02} ({days}d)")
        } else {
            format!("{days}d")
        }
    })
}

/// Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    ((if m <= 2 { y + 1 } else { y }), m as u64, d as u64)
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
        "Xhigh" => crate::app::command::reasoning_name(FrontendReasoningEffort::Xhigh),
        "Max" => crate::app::command::reasoning_name(FrontendReasoningEffort::Max),
        other => other,
    }
    .to_owned()
}

/// Dashboard effort fact: unset/default configurations render nothing.
fn effort_value(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|effort| !effort.is_empty() && *effort != "default")
        .map(effort_label)
}
