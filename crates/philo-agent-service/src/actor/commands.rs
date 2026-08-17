//! Submit, cancel, install, attach, and other command handlers.

use crate::confirmation::ConfirmationSubmit;
use crate::frontend::command::{DetachReason, FrontendAttachment, FrontendReasoningEffort};
use crate::frontend::snapshot::ServiceHealth;
use crate::frontend::update::FrontendUpdateKind;
use crate::generation::{AssembleError, AssembleRequest, AssembledGeneration};
use crate::ids::{FrontendEpoch, FrontendInstanceId, FrontendRequestId};
use crate::mapping;
use crate::runtime_api::{RuntimeEvents, RuntimePort};
use philo_agent_runtime::{CancelResult, CompactionSpec, MaintenanceId, OperationSpec};

use super::{AgentServiceActor, ServiceTaskResult};

impl<R, S> AgentServiceActor<R, S>
where
    R: RuntimePort,
    S: RuntimeEvents,
{
    pub(super) fn handle_submit(
        &mut self,
        request_id: FrontendRequestId,
        session_id: String,
        draft: String,
        attachments: Vec<FrontendAttachment>,
    ) {
        if !self.is_accepting_work() {
            self.reject_not_accepting(request_id);
            return;
        }
        if !self.can_spawn_work() {
            self.reject_child_capacity(request_id);
            return;
        }
        let user_message = match mapping::user_message(&draft, &attachments) {
            Ok(message) => message,
            Err(reason) => {
                self.emit(
                    Some(request_id),
                    FrontendUpdateKind::CommandRejected { reason },
                );
                return;
            }
        };
        if self.current_session.is_none() {
            self.current_session = Some(session_id.clone());
        }
        let spec = OperationSpec {
            session_id: mapping::session_runtime_id(&session_id),
            user_message,
            generation: self.generation.current(),
            service_request_id: Some(request_id.to_string()),
        };
        self.emit(Some(request_id), FrontendUpdateKind::CommandAccepted);
        let runtime = self.runtime.clone();
        let epoch = self.epoch;
        self.spawn_work(request_id, async move {
            let result = runtime.submit(spec).await;
            ServiceTaskResult::Submit {
                request_id,
                epoch,
                result,
            }
        });
    }

    pub(super) fn handle_set_reasoning(
        &mut self,
        request_id: FrontendRequestId,
        effort: FrontendReasoningEffort,
    ) {
        let next = self
            .generation
            .install_reasoning(mapping::reasoning_effort(effort));
        self.emit(
            Some(request_id),
            FrontendUpdateKind::GenerationInstalled {
                display: mapping::frontend_generation(&next),
            },
        );
    }

    pub(super) fn handle_confirmation_submit(&mut self, submit: ConfirmationSubmit) {
        if let Ok((confirmation_id, request)) = self.confirmations.insert(submit) {
            self.emit(
                None,
                FrontendUpdateKind::ConfirmationRequested {
                    confirmation_id,
                    title: request.title,
                    body: request.body,
                },
            );
        }
    }

    pub(super) fn handle_install_model(&mut self, request_id: FrontendRequestId, name: String) {
        if !self.is_accepting_work() {
            self.reject_not_accepting(request_id);
            return;
        }
        if !self.can_spawn_work() {
            self.reject_child_capacity(request_id);
            return;
        }
        self.generation.note_install(request_id);
        self.spawn_install(request_id, name);
    }

    pub(super) fn handle_frontend_attached(
        &mut self,
        request_id: FrontendRequestId,
        frontend_instance_id: FrontendInstanceId,
    ) {
        if let Some(previous) = self.attached.clone() {
            if previous != frontend_instance_id {
                self.detach_attached(previous, DetachReason::Replaced);
            }
        }
        self.attached = Some(frontend_instance_id);
        self.health = ServiceHealth::Ok;
        self.emit(
            Some(request_id),
            FrontendUpdateKind::ServiceHealthChanged {
                health: self.health.clone(),
            },
        );
    }

    pub(super) fn handle_frontend_detached(
        &mut self,
        request_id: FrontendRequestId,
        frontend_instance_id: FrontendInstanceId,
        reason: DetachReason,
    ) {
        if self.attached.as_ref() == Some(&frontend_instance_id) {
            self.detach_attached(frontend_instance_id, reason);
        }
        self.emit(Some(request_id), FrontendUpdateKind::CommandAccepted);
    }

    fn detach_attached(&mut self, frontend_instance_id: FrontendInstanceId, reason: DetachReason) {
        if self.attached.as_ref() != Some(&frontend_instance_id) {
            return;
        }
        self.attached = None;
        match reason {
            DetachReason::Replaced
            | DetachReason::UserExit
            | DetachReason::Restart
            | DetachReason::Fault { .. } => self.deny_all_confirmations(),
        }
    }

    pub(super) fn handle_install(
        &mut self,
        request_id: FrontendRequestId,
        epoch: FrontendEpoch,
        name: String,
        result: Result<AssembledGeneration, AssembleError>,
    ) {
        if epoch != self.epoch {
            return;
        }
        match result {
            Ok(assembled) => {
                if let Some(next) = self.generation.install_success(request_id, assembled) {
                    self.emit(
                        Some(request_id),
                        FrontendUpdateKind::GenerationInstalled {
                            display: mapping::frontend_generation(&next),
                        },
                    );
                }
            }
            Err(error) => {
                if self.generation.install_failure(request_id) {
                    self.notice(format!("generation install failed: {}", error.message));
                    self.emit(
                        Some(request_id),
                        FrontendUpdateKind::GenerationInstallFailed {
                            name,
                            message: error.message,
                        },
                    );
                }
            }
        }
    }

    pub(super) fn emit_cancel(&mut self, request_id: FrontendRequestId, result: CancelResult) {
        match result {
            CancelResult::Requested | CancelResult::QueuedCancelled => {
                self.emit(Some(request_id), FrontendUpdateKind::CommandAccepted);
            }
            other => self.emit(
                Some(request_id),
                FrontendUpdateKind::CommandRejected {
                    reason: format!("{other:?}"),
                },
            ),
        }
    }

    fn spawn_install(&mut self, request_id: FrontendRequestId, name: String) {
        let assembler = self.assembler.clone();
        let epoch = self.epoch;
        self.spawn_work(request_id, async move {
            let result = assembler
                .assemble(AssembleRequest { name: name.clone() })
                .await;
            ServiceTaskResult::Install {
                request_id,
                epoch,
                name,
                result,
            }
        });
    }

    pub(super) fn spawn_cancel(&mut self, request_id: FrontendRequestId, operation_id: String) {
        if matches!(
            self.shutdown,
            super::ServiceShutdownState::ChildrenJoining | super::ServiceShutdownState::Stopped
        ) {
            self.reject_not_accepting(request_id);
            return;
        }
        let runtime = self.runtime.clone();
        self.spawn_work(request_id, async move {
            let result = runtime
                .cancel(mapping::operation_runtime_id(&operation_id))
                .await;
            ServiceTaskResult::Cancel { request_id, result }
        });
    }

    pub(super) fn spawn_compaction(&mut self, request_id: FrontendRequestId, session_id: String) {
        if !self.is_accepting_work() {
            self.reject_not_accepting(request_id);
            return;
        }
        let spec = CompactionSpec {
            session_id: mapping::session_runtime_id(&session_id),
            generation: self.generation.current(),
        };
        let runtime = self.runtime.clone();
        self.spawn_work(request_id, async move {
            let result = runtime.start_compaction(spec).await;
            ServiceTaskResult::Compaction { request_id, result }
        });
    }

    pub(super) fn spawn_cancel_maintenance(
        &mut self,
        request_id: FrontendRequestId,
        maintenance_id: String,
    ) {
        if matches!(
            self.shutdown,
            super::ServiceShutdownState::ChildrenJoining | super::ServiceShutdownState::Stopped
        ) {
            self.reject_not_accepting(request_id);
            return;
        }
        let runtime = self.runtime.clone();
        self.spawn_work(request_id, async move {
            let result = runtime
                .cancel_maintenance(MaintenanceId::new(maintenance_id))
                .await;
            ServiceTaskResult::CancelMaintenance { request_id, result }
        });
    }
}
