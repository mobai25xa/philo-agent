//! Fenced snapshot composition. Durable view and live state share one barrier.

use std::collections::{HashMap, HashSet};

use crate::error::CommandReject;
use crate::frontend::snapshot::{
    DurableSessionView, FrontendAvailability, FrontendSnapshot, QueuedOperationSummary,
    ServiceHealth,
};
use crate::frontend::update::FrontendUpdateKind;
use crate::ids::{FrontendEpoch, FrontendRequestId};
use crate::live::LiveOperationSnapshot;
use crate::mapping;
use crate::runtime_api::{RuntimeEvents, RuntimePort};
use philo_agent_runtime::{RuntimeSnapshot, SettlementDurability, SettlementRevision};

use super::{AgentServiceActor, ServiceTaskResult, ViewKind};

/// Same-session floor-insufficient reloads are bounded. Successful publish
/// and a new `begin_pending_load` reset the counter.
pub(crate) const SNAPSHOT_RELOAD_ATTEMPT_MAX: u32 = 8;

/// Causal barrier captured when a session load or snapshot read starts.
#[derive(Clone, Debug)]
pub(crate) struct SnapshotLoadToken {
    generation: u64,
    session_id: String,
    requested_floor: u64,
    request_id: Option<FrontendRequestId>,
    frontend_epoch: FrontendEpoch,
    /// Service-local live cursor at capture. The view is stale when the actor's
    /// cursor is greater; this is never a substitute for the session floor.
    live_cursor: u64,
    active_operation_id: Option<String>,
    active_turn_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionViewKind {
    Load,
    Snapshot,
}

impl SnapshotLoadToken {
    fn live_projection_stale(
        &self,
        current_live_cursor: u64,
        active_operation_id: Option<&str>,
        active_turn_id: Option<&str>,
    ) -> bool {
        debug_assert!(
            current_live_cursor >= self.live_cursor,
            "service-local live cursor must be monotonic"
        );
        current_live_cursor > self.live_cursor
            || self.active_operation_id.as_deref() != active_operation_id
            || self.active_turn_id.as_deref() != active_turn_id
    }
}

#[derive(Clone, Debug)]
struct PendingReload {
    session_id: String,
    kind: SessionViewKind,
    request_id: Option<FrontendRequestId>,
}

#[derive(Clone, Debug)]
struct PendingLoad {
    session_id: String,
    #[allow(dead_code)]
    generation: u64,
}

enum TokenDecision {
    Drop,
    Reload,
    Publish,
}

enum SettledApply {
    Applied,
    ProtocolError { message: String },
}

/// Session-scoped snapshot ownership, floors, and in-flight load tracking.
pub(crate) struct SnapshotState {
    pub current_session: Option<String>,
    pending_load: Option<PendingLoad>,
    operation_session: HashMap<String, String>,
    required_revision: HashMap<String, u64>,
    published_revision: HashMap<String, u64>,
    load_generation: u64,
    inflight: HashSet<String>,
    pending_reload: Option<PendingReload>,
    reload_attempts: HashMap<String, u32>,
}

impl SnapshotState {
    pub(crate) fn new() -> Self {
        Self {
            current_session: None,
            pending_load: None,
            operation_session: HashMap::new(),
            required_revision: HashMap::new(),
            published_revision: HashMap::new(),
            load_generation: 0,
            inflight: HashSet::new(),
            pending_reload: None,
            reload_attempts: HashMap::new(),
        }
    }

    pub(crate) fn begin_pending_load(&mut self, session_id: String) -> u64 {
        self.reset_reload_attempts(&session_id);
        let generation = self.bump_generation();
        self.pending_load = Some(PendingLoad {
            session_id,
            generation,
        });
        generation
    }

    pub(crate) fn commit_current(&mut self, session_id: String) {
        if self
            .pending_load
            .as_ref()
            .is_some_and(|pending| pending.session_id == session_id)
        {
            self.pending_load = None;
        }
        self.current_session = Some(session_id);
    }

    pub(crate) fn has_pending_load(&self) -> bool {
        self.pending_load.is_some()
    }

    pub(crate) fn pending_load_session(&self) -> Option<&str> {
        self.pending_load
            .as_ref()
            .map(|pending| pending.session_id.as_str())
    }

    pub(crate) fn pending_load_is(&self, session_id: &str) -> bool {
        self.pending_load
            .as_ref()
            .is_some_and(|pending| pending.session_id == session_id)
    }

    pub(crate) fn clear_pending_load_if(&mut self, session_id: &str) {
        if self.pending_load_is(session_id) {
            self.pending_load = None;
        }
    }

    pub(crate) fn bump_generation(&mut self) -> u64 {
        self.load_generation = self.load_generation.saturating_add(1);
        self.load_generation
    }

    pub(crate) fn on_epoch_reset(&mut self) {
        self.bump_generation();
        self.pending_reload = None;
        self.pending_load = None;
        self.reload_attempts.clear();
    }

    fn reset_reload_attempts(&mut self, session_id: &str) {
        self.reload_attempts.remove(session_id);
    }

    fn begin_reload_attempt(&mut self, session_id: &str) -> bool {
        let count = self
            .reload_attempts
            .entry(session_id.to_owned())
            .or_insert(0);
        if *count >= SNAPSHOT_RELOAD_ATTEMPT_MAX {
            return false;
        }
        *count = count.saturating_add(1);
        true
    }

    pub(crate) fn required_for(&self, session_id: &str) -> u64 {
        self.required_revision.get(session_id).copied().unwrap_or(0)
    }

    fn published_for(&self, session_id: &str) -> u64 {
        self.published_revision
            .get(session_id)
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn is_current_session(&self, session_id: &str) -> bool {
        self.current_session.as_deref() == Some(session_id)
    }

    #[cfg(test)]
    fn load_gate_allows(&self, generation: u64, session_id: &str) -> bool {
        generation == self.load_generation && self.pending_load_is(session_id)
    }

    pub(crate) fn note_accepted(&mut self, operation_id: String, session_id: String) {
        self.operation_session.insert(operation_id, session_id);
    }

    pub(crate) fn session_of(&self, operation_id: &str) -> Option<&str> {
        self.operation_session.get(operation_id).map(String::as_str)
    }

    /// Live belongs in the current session snapshot only when ownership matches.
    pub(crate) fn live_belongs_to_current(&self, operation_id: Option<&str>) -> bool {
        match (self.current_session.as_deref(), operation_id) {
            (Some(current), Some(operation_id)) => self.session_of(operation_id) == Some(current),
            _ => false,
        }
    }

    fn apply_settled(
        &mut self,
        operation_id: &str,
        session_id: &str,
        durability: SettlementDurability,
        session_revision: SettlementRevision,
    ) -> SettledApply {
        match self.operation_session.get(operation_id).map(String::as_str) {
            None => SettledApply::ProtocolError {
                message: format!(
                    "protocol error: settled operation {operation_id} has no accepted session"
                ),
            },
            Some(owned) if owned != session_id => SettledApply::ProtocolError {
                message: format!(
                    "protocol error: settlement session {session_id} does not match accepted session {owned}"
                ),
            },
            Some(_) => {
                self.operation_session.remove(operation_id);
                if durability == SettlementDurability::Confirmed
                    && let SettlementRevision::Committed(revision) = session_revision
                {
                    let required = self
                        .required_revision
                        .entry(session_id.to_owned())
                        .or_insert(0);
                    *required = (*required).max(revision.get());
                }
                SettledApply::Applied
            }
        }
    }

    fn note_published(&mut self, session_id: &str, revision: u64) {
        let published = self
            .published_revision
            .entry(session_id.to_owned())
            .or_insert(0);
        *published = (*published).max(revision);
    }

    fn capture_token(
        &self,
        session_id: String,
        request_id: Option<FrontendRequestId>,
        frontend_epoch: FrontendEpoch,
        live_cursor: u64,
        active_operation_id: Option<String>,
        active_turn_id: Option<String>,
    ) -> SnapshotLoadToken {
        let requested_floor = self.required_for(&session_id);
        SnapshotLoadToken {
            generation: self.load_generation,
            requested_floor,
            session_id,
            request_id,
            frontend_epoch,
            live_cursor,
            active_operation_id,
            active_turn_id,
        }
    }

    fn begin_inflight(&mut self, pending: PendingReload) -> (bool, Option<FrontendRequestId>) {
        if self.inflight.contains(&pending.session_id) {
            let superseded = self
                .pending_reload
                .replace(pending)
                .and_then(|pending| pending.request_id);
            return (false, superseded);
        }
        self.inflight.insert(pending.session_id.clone());
        (true, None)
    }

    fn end_inflight(&mut self, session_id: &str) -> Option<PendingReload> {
        self.inflight.remove(session_id);
        match self.pending_reload.take() {
            Some(pending) if pending.session_id == session_id => Some(pending),
            other => {
                self.pending_reload = other;
                None
            }
        }
    }
}

impl<R, S> AgentServiceActor<R, S>
where
    R: RuntimePort,
    S: RuntimeEvents,
{
    pub(super) fn handle_request_snapshot(&mut self, request_id: FrontendRequestId) {
        if !self.is_accepting_work() {
            self.reject_not_accepting(request_id);
            return;
        }
        self.begin_fenced_snapshot(Some(request_id));
    }

    pub(super) fn handle_preview_session(
        &mut self,
        request_id: FrontendRequestId,
        session_id: String,
        request_generation: u64,
    ) {
        if !self.is_accepting_work() {
            self.reject_not_accepting(request_id);
            return;
        }
        if !self.can_spawn_work() {
            self.reject_child_capacity(request_id);
            return;
        }
        self.latest_preview = Some((request_id, request_generation));
        self.emit(Some(request_id), FrontendUpdateKind::CommandAccepted);
        self.spawn_view(
            request_id,
            ViewKind::Preview { request_generation },
            session_id,
        );
    }

    pub(super) fn start_session_load(&mut self, request_id: FrontendRequestId, session_id: String) {
        self.snapshot.begin_pending_load(session_id.clone());
        self.spawn_tracked_view(Some(request_id), SessionViewKind::Load, session_id);
    }

    pub(super) fn spawn_view(
        &mut self,
        request_id: FrontendRequestId,
        kind: ViewKind,
        session_id: String,
    ) {
        let sessions = self.sessions.clone();
        let epoch = self.epoch;
        self.spawn_work(request_id, async move {
            let store_id = mapping::session_store_id(&session_id);
            let result = sessions
                .context_view(&store_id)
                .await
                .map_err(|error| format!("{error:?}"));
            ServiceTaskResult::StoreView {
                request_id,
                epoch,
                kind,
                session_id,
                result,
            }
        });
    }

    pub(super) fn handle_store_view(
        &mut self,
        request_id: FrontendRequestId,
        epoch: FrontendEpoch,
        kind: ViewKind,
        session_id: String,
        result: Result<philo_session::SessionContextView, String>,
    ) {
        let pending = match &kind {
            ViewKind::Snapshot(token) | ViewKind::Load(token) => {
                self.snapshot.end_inflight(&token.session_id)
            }
            ViewKind::Preview { .. } => None,
        };
        if epoch != self.epoch {
            self.feed.cancel_request(request_id);
            return;
        }
        let view = match result {
            Ok(view) => view,
            Err(reason) => {
                self.emit(
                    Some(request_id),
                    FrontendUpdateKind::CommandRejected {
                        reason: CommandReject::InvalidInput { reason },
                    },
                );
                if let ViewKind::Load(token) = &kind {
                    self.snapshot.clear_pending_load_if(&token.session_id);
                    return;
                }
                self.maybe_schedule_pending(pending);
                return;
            }
        };
        match kind {
            ViewKind::Snapshot(token) => {
                self.finish_session_view(token, view, SessionViewKind::Snapshot, pending)
            }
            ViewKind::Load(token) => {
                self.finish_session_view(token, view, SessionViewKind::Load, pending)
            }
            ViewKind::Preview { request_generation } => {
                if self.latest_preview.is_some_and(|(latest_id, latest_gen)| {
                    request_id < latest_id || request_generation < latest_gen
                }) {
                    self.feed.cancel_request(request_id);
                    return;
                }
                self.emit(
                    Some(request_id),
                    FrontendUpdateKind::SessionPreviewed {
                        session_id,
                        view: mapping::durable_session_view(&view),
                    },
                );
            }
        }
    }

    pub(super) fn apply_operation_settled(
        &mut self,
        operation_id: &str,
        session_id: &str,
        durability: SettlementDurability,
        session_revision: SettlementRevision,
    ) -> bool {
        match self
            .snapshot
            .apply_settled(operation_id, session_id, durability, session_revision)
        {
            SettledApply::Applied => true,
            SettledApply::ProtocolError { message } => {
                self.notice(message.clone());
                self.health = ServiceHealth::Degraded {
                    message: message.clone(),
                };
                self.emit(
                    None,
                    FrontendUpdateKind::ServiceHealthChanged {
                        health: self.health.clone(),
                    },
                );
                false
            }
        }
    }

    pub(super) fn on_subscription_lagged(&mut self) {
        self.live.mark_lagged();
        self.feed.force_resync();
        self.request_runtime_snapshot();
    }

    pub(super) fn handle_runtime_snapshot(
        &mut self,
        epoch: FrontendEpoch,
        snapshot: RuntimeSnapshot,
    ) {
        self.runtime_snapshot_inflight = false;
        if epoch != self.epoch {
            return;
        }
        let applied = self.apply_runtime_snapshot(snapshot);
        if self.feed.is_resyncing() {
            self.begin_fenced_snapshot(self.latest_snapshot);
        } else if applied {
            self.emit(
                None,
                FrontendUpdateKind::AvailabilityChanged {
                    availability: self.availability.clone(),
                    queued: self.queued.len(),
                },
            );
        }
    }

    pub(super) fn begin_fenced_snapshot(&mut self, request_id: Option<FrontendRequestId>) {
        self.snapshot.bump_generation();
        self.latest_snapshot = request_id;
        match self.snapshot.current_session.clone() {
            Some(session_id) => {
                self.snapshot.reset_reload_attempts(&session_id);
                self.spawn_tracked_view(request_id, SessionViewKind::Snapshot, session_id);
            }
            None => {
                let snapshot = self.compose_snapshot(None, self.live.clone());
                self.emit_snapshot(request_id, snapshot);
            }
        }
    }

    fn spawn_tracked_view(
        &mut self,
        request_id: Option<FrontendRequestId>,
        kind: SessionViewKind,
        session_id: String,
    ) {
        let pending = PendingReload {
            session_id: session_id.clone(),
            kind,
            request_id,
        };
        let (started, superseded) = self.snapshot.begin_inflight(pending);
        if let Some(request_id) = superseded.filter(|id| Some(*id) != request_id) {
            self.feed.cancel_request(request_id);
        }
        if !started {
            return;
        }
        let spawn_id = request_id.unwrap_or(FrontendRequestId::INVALID);
        if !self.can_spawn_work() {
            self.snapshot.end_inflight(&session_id);
            if kind == SessionViewKind::Load {
                self.snapshot.clear_pending_load_if(&session_id);
            }
            if let Some(id) = request_id {
                self.reject_child_capacity(id);
            }
            return;
        }
        let token = self.capture_token(request_id, &session_id);
        let view_kind = match kind {
            SessionViewKind::Load => ViewKind::Load(token),
            SessionViewKind::Snapshot => ViewKind::Snapshot(token),
        };
        self.spawn_view(spawn_id, view_kind, session_id);
    }

    fn capture_token(
        &self,
        request_id: Option<FrontendRequestId>,
        session_id: &str,
    ) -> SnapshotLoadToken {
        self.snapshot.capture_token(
            session_id.to_owned(),
            request_id,
            self.epoch,
            self.live_cursor,
            self.live.operation_id.clone(),
            self.live.turn_id.clone(),
        )
    }

    fn finish_session_view(
        &mut self,
        token: SnapshotLoadToken,
        view: philo_session::SessionContextView,
        kind: SessionViewKind,
        pending: Option<PendingReload>,
    ) {
        match self.evaluate_token(&token, view.revision().get(), kind) {
            TokenDecision::Drop => {
                let pending_request = pending.as_ref().and_then(|pending| pending.request_id);
                if token.request_id != pending_request
                    && let Some(request_id) = token.request_id
                {
                    self.feed.cancel_request(request_id);
                }
                self.maybe_schedule_pending(pending);
            }
            TokenDecision::Reload => {
                let reload = if kind == SessionViewKind::Load
                    && self.snapshot.pending_load_is(&token.session_id)
                {
                    PendingReload {
                        session_id: token.session_id,
                        kind: SessionViewKind::Load,
                        request_id: token.request_id,
                    }
                } else {
                    pending.unwrap_or(PendingReload {
                        session_id: token.session_id,
                        kind,
                        request_id: token.request_id,
                    })
                };
                self.schedule_reload(reload);
            }
            TokenDecision::Publish => {
                self.publish_session_view(token, view, kind);
                self.maybe_schedule_pending(pending);
            }
        }
    }

    fn evaluate_token(
        &self,
        token: &SnapshotLoadToken,
        view_revision: u64,
        kind: SessionViewKind,
    ) -> TokenDecision {
        if token.frontend_epoch != self.epoch {
            return TokenDecision::Drop;
        }
        match kind {
            SessionViewKind::Load => {
                if !self.snapshot.pending_load_is(&token.session_id) {
                    return TokenDecision::Drop;
                }
                if token.generation != self.snapshot.load_generation {
                    return TokenDecision::Reload;
                }
            }
            SessionViewKind::Snapshot => {
                if token.generation != self.snapshot.load_generation
                    || !self.snapshot.is_current_session(&token.session_id)
                {
                    return TokenDecision::Drop;
                }
                if self.latest_snapshot != token.request_id {
                    return TokenDecision::Drop;
                }
            }
        }
        let required = self.snapshot.required_for(&token.session_id);
        debug_assert!(
            required >= token.requested_floor,
            "session floor must be monotonic"
        );
        if view_revision < required {
            return TokenDecision::Reload;
        }
        if view_revision < self.snapshot.published_for(&token.session_id) {
            return TokenDecision::Drop;
        }
        if kind == SessionViewKind::Snapshot && self.snapshot_live_stale(token) {
            return TokenDecision::Reload;
        }
        TokenDecision::Publish
    }

    fn snapshot_live_stale(&self, token: &SnapshotLoadToken) -> bool {
        token.live_projection_stale(
            self.live_cursor,
            self.live.operation_id.as_deref(),
            self.live.turn_id.as_deref(),
        )
    }

    fn publish_session_view(
        &mut self,
        token: SnapshotLoadToken,
        view: philo_session::SessionContextView,
        kind: SessionViewKind,
    ) {
        self.snapshot
            .note_published(&token.session_id, view.revision().get());
        self.snapshot.reset_reload_attempts(&token.session_id);
        match kind {
            SessionViewKind::Load => {
                self.snapshot.commit_current(token.session_id.clone());
                self.drop_foreign_live();
                // Pre-fill the per-session generation cache so the next submit
                // uses a bound generation. Hot sessions already in the cache
                // keep their Arc; cold sessions fall back to the bootstrap
                // generation until the user reinstalls the persisted model.
                if self.session_generations.get(&token.session_id).is_none() {
                    self.session_generations
                        .put(token.session_id.clone(), self.generation.current());
                }
                self.emit(
                    token.request_id,
                    FrontendUpdateKind::SessionLoaded {
                        session_id: token.session_id,
                        view: mapping::durable_session_view(&view),
                    },
                );
            }
            SessionViewKind::Snapshot => {
                let durable = mapping::durable_session_view(&view);
                let live = self.live_for_snapshot(&view);
                let snapshot = self.compose_snapshot(Some(durable), live);
                self.emit_snapshot(token.request_id, snapshot);
            }
        }
    }

    fn maybe_schedule_pending(&mut self, pending: Option<PendingReload>) {
        let Some(pending) = pending else {
            return;
        };
        self.schedule_reload(pending);
    }

    fn schedule_reload(&mut self, pending: PendingReload) {
        if !self.is_accepting_work() {
            if let Some(request_id) = pending.request_id {
                self.feed.cancel_request(request_id);
            }
            return;
        }
        let allowed = match pending.kind {
            SessionViewKind::Load => self.snapshot.pending_load_is(&pending.session_id),
            SessionViewKind::Snapshot => self.snapshot.is_current_session(&pending.session_id),
        };
        if !allowed {
            if let Some(request_id) = pending.request_id {
                self.feed.cancel_request(request_id);
            }
            return;
        }
        if !self.snapshot.begin_reload_attempt(&pending.session_id) {
            self.notice(format!(
                "snapshot reload limit reached for session {}",
                pending.session_id
            ));
            match pending.kind {
                SessionViewKind::Load => {
                    self.snapshot.clear_pending_load_if(&pending.session_id);
                    if let Some(request_id) = pending.request_id {
                        self.emit(
                            Some(request_id),
                            FrontendUpdateKind::CommandRejected {
                                reason: CommandReject::InvalidInput {
                                    reason: format!(
                                        "session load reload limit reached for {}",
                                        pending.session_id
                                    ),
                                },
                            },
                        );
                    }
                }
                SessionViewKind::Snapshot => {
                    if let Some(request_id) = pending.request_id {
                        self.emit(
                            Some(request_id),
                            FrontendUpdateKind::CommandRejected {
                                reason: CommandReject::InvalidInput {
                                    reason: format!(
                                        "snapshot reload limit reached for {}",
                                        pending.session_id
                                    ),
                                },
                            },
                        );
                    }
                }
            }
            return;
        }
        self.snapshot.bump_generation();
        self.spawn_tracked_view(pending.request_id, pending.kind, pending.session_id);
    }

    fn live_for_snapshot(&self, view: &philo_session::SessionContextView) -> LiveOperationSnapshot {
        let mut live = self.filter_live_for_current(self.live.clone());
        if mapping::session_view_covers_live(view, &live) {
            live.settle();
        }
        live
    }

    fn filter_live_for_current(&self, live: LiveOperationSnapshot) -> LiveOperationSnapshot {
        if self
            .snapshot
            .live_belongs_to_current(live.operation_id.as_deref())
        {
            live
        } else {
            LiveOperationSnapshot::default()
        }
    }

    fn drop_foreign_live(&mut self) {
        if !self
            .snapshot
            .live_belongs_to_current(self.live.operation_id.as_deref())
        {
            self.live.clear();
        }
    }

    pub(super) fn live_accepts_agent_events(&self) -> bool {
        self.snapshot
            .live_belongs_to_current(self.live.operation_id.as_deref())
    }

    pub(super) fn compose_snapshot(
        &self,
        durable_session_view: Option<DurableSessionView>,
        live: LiveOperationSnapshot,
    ) -> FrontendSnapshot {
        debug_assert!(
            durable_session_view.is_some() || self.snapshot.current_session.is_none(),
            "must not publish current_session_id: Some with durable_session_view: None"
        );
        let live = self.filter_live_for_current(live);
        FrontendSnapshot {
            epoch: self.epoch,
            revision: self.revision,
            current_session_id: self.snapshot.current_session.clone(),
            durable_session_view,
            usage: live.usage,
            live,
            queued: self.queued.clone(),
            maintenance: self.maintenance.clone(),
            availability: self.availability.clone(),
            generation: self.generation.display(),
            pending_confirmations: self.confirmations.views(),
            config_notices: self.notices.clone(),
            health: self.health.clone(),
        }
    }

    pub(super) fn emit_snapshot(
        &mut self,
        request_id: Option<FrontendRequestId>,
        snapshot: FrontendSnapshot,
    ) {
        let result = self.emit_result(
            request_id,
            FrontendUpdateKind::SnapshotReady(Box::new(snapshot)),
        );
        self.feed.on_snapshot_ready(result);
    }

    fn request_runtime_snapshot(&mut self) {
        if self.runtime_snapshot_inflight || !self.can_spawn_work() {
            return;
        }
        self.runtime_snapshot_inflight = true;
        let runtime = self.runtime.clone();
        let epoch = self.epoch;
        self.spawn_child(None, async move {
            let snapshot = runtime.snapshot().await;
            ServiceTaskResult::RuntimeSnapshot { epoch, snapshot }
        });
    }

    fn apply_runtime_snapshot(&mut self, snapshot: RuntimeSnapshot) -> bool {
        if snapshot.runtime_revision < self.applied_runtime_snapshot_revision {
            return false;
        }
        self.applied_runtime_snapshot_revision = snapshot.runtime_revision;
        self.live_cursor = self.live_cursor.saturating_add(1);
        self.availability = mapping::availability(&snapshot.availability);
        self.queued = snapshot
            .queued
            .iter()
            .map(|queued| QueuedOperationSummary {
                operation_id: queued.operation_id.to_string(),
                session_id: queued.session_id.to_string(),
            })
            .collect();
        self.maintenance = snapshot.maintenance.map(|maintenance| {
            crate::frontend::snapshot::FrontendMaintenance {
                id: maintenance.id.to_string(),
                phase: crate::frontend::snapshot::FrontendMaintenancePhase::Started,
                message: None,
            }
        });
        if let Some(active) = snapshot.active {
            if self.snapshot.is_current_session(active.session_id.as_str()) {
                self.snapshot.note_accepted(
                    active.operation_id.to_string(),
                    active.session_id.to_string(),
                );
                self.live.start_operation(active.operation_id.as_str());
                self.live.start_turn(active.turn_id.as_str());
            }
        } else if let FrontendAvailability::Busy { operation_id } = &self.availability {
            let operation_id = operation_id.clone();
            if self
                .snapshot
                .live_belongs_to_current(Some(operation_id.as_str()))
                && self.live.operation_id.as_deref() != Some(operation_id.as_str())
            {
                self.live.start_operation(operation_id);
            }
        }
        self.drop_foreign_live();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::SNAPSHOT_RELOAD_ATTEMPT_MAX;
    use super::SnapshotState;
    use crate::ids::FrontendEpoch;
    use philo_agent_runtime::{SettlementDurability, SettlementRevision};
    use philo_session::SessionRevision;

    #[test]
    fn begin_pending_load_does_not_change_current() {
        let mut state = SnapshotState::new();
        state.commit_current("sess-a".into());
        let first = state.begin_pending_load("sess-b".into());
        let second = state.begin_pending_load("sess-c".into());
        assert!(second > first);
        assert_eq!(state.current_session.as_deref(), Some("sess-a"));
        assert!(state.pending_load_is("sess-c"));
        assert!(!state.pending_load_is("sess-b"));
    }

    #[test]
    fn commit_current_clears_matching_pending() {
        let mut state = SnapshotState::new();
        let _ = state.begin_pending_load("sess-a".into());
        state.commit_current("sess-a".into());
        assert_eq!(state.current_session.as_deref(), Some("sess-a"));
        assert!(!state.has_pending_load());

        let _ = state.begin_pending_load("sess-b".into());
        state.commit_current("sess-a".into());
        assert_eq!(state.current_session.as_deref(), Some("sess-a"));
        assert!(state.pending_load_is("sess-b"));
    }

    #[test]
    fn live_token_staleness_is_monotonic_and_identity_sensitive() {
        let state = SnapshotState::new();
        let token = state.capture_token(
            "sess-a".into(),
            None,
            FrontendEpoch::INITIAL,
            7,
            Some("op-a".into()),
            Some("turn-a".into()),
        );

        assert!(!token.live_projection_stale(7, Some("op-a"), Some("turn-a")));
        assert!(token.live_projection_stale(8, Some("op-a"), Some("turn-a")));
        assert!(token.live_projection_stale(7, Some("op-b"), Some("turn-a")));
        assert!(token.live_projection_stale(7, Some("op-a"), Some("turn-b")));
    }

    #[test]
    fn load_gate_rejects_mismatched_generation_or_session() {
        let mut state = SnapshotState::new();
        let generation = state.begin_pending_load("sess-a".into());
        assert!(state.load_gate_allows(generation, "sess-a"));
        assert!(!state.load_gate_allows(generation.saturating_sub(1), "sess-a"));
        assert!(!state.load_gate_allows(generation, "sess-b"));
        state.commit_current("sess-a".into());
        assert!(!state.load_gate_allows(generation, "sess-a"));
    }

    #[test]
    fn reload_attempts_are_bounded_and_reset_on_new_load() {
        let mut state = SnapshotState::new();
        for _ in 0..SNAPSHOT_RELOAD_ATTEMPT_MAX {
            assert!(state.begin_reload_attempt("sess-a"));
        }
        assert!(!state.begin_reload_attempt("sess-a"));
        assert!(state.begin_reload_attempt("sess-b"));
        let _ = state.begin_pending_load("sess-a".into());
        assert!(state.begin_reload_attempt("sess-a"));
    }

    #[test]
    fn floor_is_monotonic_and_session_scoped() {
        let mut state = SnapshotState::new();
        state.note_accepted("op-a".into(), "sess-a".into());
        state.note_accepted("op-b".into(), "sess-b".into());
        assert!(matches!(
            state.apply_settled(
                "op-a",
                "sess-a",
                SettlementDurability::Confirmed,
                SettlementRevision::Committed(SessionRevision::new(2)),
            ),
            super::SettledApply::Applied
        ));
        assert!(matches!(
            state.apply_settled(
                "op-b",
                "sess-b",
                SettlementDurability::Confirmed,
                SettlementRevision::Committed(SessionRevision::new(9)),
            ),
            super::SettledApply::Applied
        ));
        assert_eq!(state.required_for("sess-a"), 2);
        assert_eq!(state.required_for("sess-b"), 9);

        state.note_accepted("op-a2".into(), "sess-a".into());
        assert!(matches!(
            state.apply_settled(
                "op-a2",
                "sess-a",
                SettlementDurability::Confirmed,
                SettlementRevision::Committed(SessionRevision::new(1)),
            ),
            super::SettledApply::Applied
        ));
        assert_eq!(state.required_for("sess-a"), 2);
    }

    #[test]
    fn live_belongs_to_current_requires_matching_session() {
        let mut state = SnapshotState::new();
        state.commit_current("sess-b".into());
        state.note_accepted("op-a".into(), "sess-a".into());
        state.note_accepted("op-b".into(), "sess-b".into());
        assert!(!state.live_belongs_to_current(Some("op-a")));
        assert!(state.live_belongs_to_current(Some("op-b")));
        assert!(!state.live_belongs_to_current(None));
        assert!(!state.live_belongs_to_current(Some("op-missing")));
    }

    #[test]
    fn mismatch_and_missing_ownership_do_not_raise_floor() {
        let mut state = SnapshotState::new();
        state.note_accepted("op-a".into(), "sess-a".into());
        let mismatch = state.apply_settled(
            "op-a",
            "sess-b",
            SettlementDurability::Confirmed,
            SettlementRevision::Committed(SessionRevision::new(7)),
        );
        assert!(matches!(
            mismatch,
            super::SettledApply::ProtocolError { .. }
        ));
        assert_eq!(state.required_for("sess-a"), 0);
        assert_eq!(state.required_for("sess-b"), 0);
        assert_eq!(state.session_of("op-a"), Some("sess-a"));
        assert!(matches!(
            state.apply_settled(
                "op-a",
                "sess-a",
                SettlementDurability::Confirmed,
                SettlementRevision::Unchanged,
            ),
            super::SettledApply::Applied
        ));
        assert_eq!(state.session_of("op-a"), None);

        let missing = state.apply_settled(
            "op-missing",
            "sess-a",
            SettlementDurability::Confirmed,
            SettlementRevision::Committed(SessionRevision::new(9)),
        );
        assert!(matches!(missing, super::SettledApply::ProtocolError { .. }));
        assert_eq!(state.required_for("sess-a"), 0);
    }

    #[test]
    fn unchanged_settlement_does_not_raise_floor() {
        let mut state = SnapshotState::new();
        state.note_accepted("op-a".into(), "sess-a".into());
        assert!(matches!(
            state.apply_settled(
                "op-a",
                "sess-a",
                SettlementDurability::Confirmed,
                SettlementRevision::Unchanged,
            ),
            super::SettledApply::Applied
        ));
        assert_eq!(state.required_for("sess-a"), 0);
    }

    #[test]
    fn published_revision_is_monotonic() {
        let mut state = SnapshotState::new();
        state.note_published("sess-a", 3);
        state.note_published("sess-a", 2);
        state.note_published("sess-a", 5);
        assert_eq!(state.published_for("sess-a"), 5);
    }

    #[test]
    fn service_state_revision_and_snapshot_fence_are_deleted() {
        for source in [
            include_str!("snapshot.rs"),
            include_str!("mod.rs"),
            include_str!("commands.rs"),
            include_str!("catalog.rs"),
        ] {
            let production = source.split("#[cfg(test)]").next().unwrap_or(source);
            assert!(
                !production.contains("service_state_revision"),
                "service_state_revision must stay deleted"
            );
            assert!(
                !production.contains("SnapshotFence"),
                "SnapshotFence must stay replaced by SnapshotLoadToken"
            );
        }
    }
}
