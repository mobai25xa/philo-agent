//! Fenced snapshot composition. Durable view and live state share one barrier.

use crate::frontend::snapshot::{
    DurableSessionView, FrontendAvailability, FrontendSnapshot, QueuedOperationSummary,
};
use crate::frontend::update::FrontendUpdateKind;
use crate::ids::{FrontendEpoch, FrontendRequestId, FrontendRevision};
use crate::live::LiveOperationSnapshot;
use crate::mapping;
use crate::runtime_api::{RuntimeEvents, RuntimePort};
use philo_agent_runtime::{RuntimeSnapshot, SettlementDurability};

use super::{AgentServiceActor, ServiceTaskResult, ViewKind};

/// Causal barrier captured when a snapshot read starts.
#[derive(Clone, Debug)]
pub(crate) struct SnapshotFence {
    token: u64,
    frontend_epoch: FrontendEpoch,
    request_id: Option<FrontendRequestId>,
    session_id: Option<String>,
    #[allow(dead_code)]
    service_state_revision: FrontendRevision,
    runtime_revision: u64,
    required_session_revision: u64,
    active_operation_id: Option<String>,
    active_turn_id: Option<String>,
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
        if epoch != self.epoch {
            return;
        }
        let view = match result {
            Ok(view) => view,
            Err(reason) => {
                self.emit(
                    Some(request_id),
                    FrontendUpdateKind::CommandRejected { reason },
                );
                return;
            }
        };
        match kind {
            ViewKind::Snapshot(fence) => self.finish_fenced_snapshot(fence, view),
            ViewKind::Load => self.emit(
                Some(request_id),
                FrontendUpdateKind::SessionLoaded {
                    session_id,
                    view: mapping::durable_session_view(&view),
                },
            ),
            ViewKind::Preview { request_generation } => {
                if self.latest_preview.is_some_and(|(latest_id, latest_gen)| {
                    request_id < latest_id || request_generation < latest_gen
                }) {
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

    pub(super) fn on_operation_settled(
        &mut self,
        durability: SettlementDurability,
        session_revision: Option<u64>,
    ) {
        if durability == SettlementDurability::Confirmed {
            if let (Some(session_id), Some(revision)) =
                (self.current_session.clone(), session_revision)
            {
                let required = self
                    .required_session_revision
                    .entry(session_id)
                    .or_insert(0);
                *required = (*required).max(revision);
            }
        }
        if self.feed.is_resyncing() {
            self.begin_fenced_snapshot(self.latest_snapshot);
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
        self.apply_runtime_snapshot(snapshot);
        if self.feed.is_resyncing() {
            self.begin_fenced_snapshot(self.latest_snapshot);
        } else {
            self.emit(
                None,
                FrontendUpdateKind::AvailabilityChanged {
                    availability: self.availability.clone(),
                    queued: self.queued.len(),
                },
            );
        }
    }

    fn begin_fenced_snapshot(&mut self, request_id: Option<FrontendRequestId>) {
        self.snapshot_token = self.snapshot_token.saturating_add(1);
        self.latest_snapshot = request_id;
        let fence = self.capture_fence(request_id);
        match self.current_session.clone() {
            Some(session_id) => {
                let spawn_id = request_id.unwrap_or(FrontendRequestId::INVALID);
                if !self.can_spawn_work() {
                    if let Some(id) = request_id {
                        self.reject_child_capacity(id);
                    }
                    return;
                }
                self.spawn_view(spawn_id, ViewKind::Snapshot(fence), session_id);
            }
            None => {
                let snapshot = self.compose_snapshot(None, self.live.clone());
                self.emit_snapshot(request_id, snapshot);
            }
        }
    }

    fn capture_fence(&self, request_id: Option<FrontendRequestId>) -> SnapshotFence {
        SnapshotFence {
            token: self.snapshot_token,
            frontend_epoch: self.epoch,
            request_id,
            session_id: self.current_session.clone(),
            service_state_revision: self.revision,
            runtime_revision: self.applied_runtime_revision,
            required_session_revision: self.required_for(self.current_session.as_deref()),
            active_operation_id: self.live.operation_id.clone(),
            active_turn_id: self.live.turn_id.clone(),
        }
    }

    fn finish_fenced_snapshot(
        &mut self,
        fence: SnapshotFence,
        view: philo_session::SessionContextView,
    ) {
        if fence.token != self.snapshot_token || fence.frontend_epoch != self.epoch {
            return;
        }
        if self.latest_snapshot != fence.request_id {
            return;
        }
        if self.current_session != fence.session_id {
            return;
        }
        if self.fence_is_stale(&fence) {
            self.begin_fenced_snapshot(fence.request_id);
            return;
        }
        let required = self.required_for(fence.session_id.as_deref());
        if view.revision().get() < required {
            let Some(session_id) = fence.session_id.clone() else {
                return;
            };
            let spawn_id = fence.request_id.unwrap_or(FrontendRequestId::INVALID);
            if !self.can_spawn_work() {
                if let Some(id) = fence.request_id {
                    self.reject_child_capacity(id);
                }
                return;
            }
            self.spawn_view(spawn_id, ViewKind::Snapshot(fence), session_id);
            return;
        }
        let durable = mapping::durable_session_view(&view);
        let live = self.live_for_snapshot(&view);
        let snapshot = self.compose_snapshot(Some(durable), live);
        self.emit_snapshot(fence.request_id, snapshot);
    }

    fn fence_is_stale(&self, fence: &SnapshotFence) -> bool {
        if self.epoch != fence.frontend_epoch {
            return true;
        }
        if self.current_session != fence.session_id {
            return true;
        }
        if self.required_for(fence.session_id.as_deref()) > fence.required_session_revision {
            return true;
        }
        if self.live.operation_id != fence.active_operation_id
            || self.live.turn_id != fence.active_turn_id
        {
            return true;
        }
        if self.applied_runtime_revision < fence.runtime_revision {
            return true;
        }
        false
    }

    fn required_for(&self, session_id: Option<&str>) -> u64 {
        session_id
            .and_then(|id| self.required_session_revision.get(id).copied())
            .unwrap_or(0)
    }

    fn live_for_snapshot(&self, view: &philo_session::SessionContextView) -> LiveOperationSnapshot {
        let mut live = self.live.clone();
        if mapping::session_view_covers_live(view, &live) {
            live.settle();
        }
        live
    }

    pub(super) fn compose_snapshot(
        &self,
        durable_session_view: Option<DurableSessionView>,
        live: LiveOperationSnapshot,
    ) -> FrontendSnapshot {
        FrontendSnapshot {
            epoch: self.epoch,
            revision: self.revision,
            current_session_id: self.current_session.clone(),
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
        self.feed.on_snapshot_ready();
        self.emit(
            request_id,
            FrontendUpdateKind::SnapshotReady(Box::new(snapshot)),
        );
    }

    fn request_runtime_snapshot(&mut self) {
        if self.runtime_snapshot_inflight || !self.can_spawn_work() {
            return;
        }
        self.runtime_snapshot_inflight = true;
        let runtime = self.runtime.clone();
        let epoch = self.epoch;
        self.child_tasks.spawn(async move {
            let snapshot = runtime.snapshot().await;
            ServiceTaskResult::RuntimeSnapshot { epoch, snapshot }
        });
    }

    fn apply_runtime_snapshot(&mut self, snapshot: RuntimeSnapshot) {
        self.applied_runtime_revision =
            self.applied_runtime_revision.max(snapshot.runtime_revision);
        self.availability = mapping::availability(&snapshot.availability);
        self.queued = snapshot
            .queued
            .iter()
            .map(|operation_id| QueuedOperationSummary {
                operation_id: operation_id.to_string(),
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
            self.live.start_operation(active.operation_id.as_str());
            self.live.start_turn(active.turn_id.as_str());
        } else if let FrontendAvailability::Busy { operation_id } = &self.availability {
            let operation_id = operation_id.clone();
            if self.live.operation_id.as_deref() != Some(operation_id.as_str()) {
                self.live.start_operation(operation_id);
            }
        }
    }
}
