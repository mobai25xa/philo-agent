//! Submit, cancel, install, and other command handlers.

use crate::confirmation::ConfirmationSubmit;
use crate::error::CommandReject;
use crate::frontend::command::{FrontendAttachment, FrontendReasoningEffort};
use crate::frontend::update::FrontendUpdateKind;
use crate::generation::{AssembleError, AssembleRequest, AssembledGeneration};
use crate::ids::{FrontendEpoch, FrontendRequestId};
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
        draft: String,
        attachments: Vec<FrontendAttachment>,
    ) {
        if !self.is_accepting_work() {
            self.reject_not_accepting(request_id);
            return;
        }
        if self.snapshot.has_pending_load() {
            self.emit(
                Some(request_id),
                FrontendUpdateKind::CommandRejected {
                    reason: CommandReject::NoCurrentSession,
                },
            );
            return;
        }
        if !self.can_spawn_work() {
            self.reject_child_capacity(request_id);
            return;
        }
        let session_id = match self.snapshot.current_session.clone() {
            Some(session_id) => session_id,
            None => {
                self.emit(
                    Some(request_id),
                    FrontendUpdateKind::CommandRejected {
                        reason: CommandReject::NoCurrentSession,
                    },
                );
                return;
            }
        };
        // Image attachments are a model capability: reject them up front when
        // the active generation's model does not declare image input.
        let session_gen = self.session_generation(&session_id);
        if !attachments.is_empty() && !session_gen.display.image_input {
            let model = session_gen.display.model_name.clone();
            self.emit(
                Some(request_id),
                FrontendUpdateKind::CommandRejected {
                    reason: CommandReject::InvalidInput {
                        reason: format!("model '{model}' does not accept image attachments"),
                    },
                },
            );
            return;
        }
        let user_message = match mapping::user_message(&draft, &attachments) {
            Ok(message) => message,
            Err(reason) => {
                self.emit(
                    Some(request_id),
                    FrontendUpdateKind::CommandRejected {
                        reason: CommandReject::InvalidInput { reason },
                    },
                );
                return;
            }
        };
        let spec = OperationSpec {
            session_id: mapping::session_runtime_id(&session_id),
            user_message,
            generation: session_gen,
            service_request_id: Some(request_id.to_string()),
        };
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
        let session_id = match self.snapshot.current_session.clone() {
            Some(id) => id,
            None => {
                self.emit(
                    Some(request_id),
                    FrontendUpdateKind::CommandRejected {
                        reason: CommandReject::NoCurrentSession,
                    },
                );
                return;
            }
        };
        let current = self.session_generation(&session_id);
        let next = self
            .generation
            .install_reasoning_for(mapping::reasoning_effort(effort), &current);
        self.session_generations
            .put(session_id.clone(), next.clone());
        self.emit(
            Some(request_id),
            FrontendUpdateKind::GenerationInstalled {
                display: mapping::frontend_generation(&next),
            },
        );
    }

    pub(super) fn handle_confirmation_submit(&mut self, submit: ConfirmationSubmit) {
        if let Ok((confirmation_id, request)) = self
            .confirmations
            .insert(submit, self.current_lease_generation())
        {
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

    /// Read-only model catalog query. Like `ReadStatus`: synchronous, never
    /// spawns work, and answered in every shutdown state.
    pub(super) fn handle_list_models(&mut self, request_id: FrontendRequestId) {
        let current = self.generation.current().display.model_name.clone();
        let models = self
            .assembler
            .list_models()
            .into_iter()
            .map(|entry| crate::frontend::snapshot::FrontendModelListing {
                current: entry.id == current || entry.model == current,
                id: entry.id,
                provider: entry.provider,
                model: entry.model,
                reasoning_tiers: entry.reasoning_tiers,
            })
            .collect();
        self.emit(
            Some(request_id),
            FrontendUpdateKind::ModelListLoaded { models },
        );
    }

    pub(super) fn handle_install_model(
        &mut self,
        request_id: FrontendRequestId,
        name: String,
        effort: Option<FrontendReasoningEffort>,
    ) {
        if !self.is_accepting_work() {
            self.reject_not_accepting(request_id);
            return;
        }
        if !self.can_spawn_work() {
            self.reject_child_capacity(request_id);
            return;
        }
        self.generation.note_install(request_id);
        self.spawn_install(request_id, name, effort);
    }

    pub(super) fn handle_install(
        &mut self,
        request_id: FrontendRequestId,
        epoch: FrontendEpoch,
        name: String,
        result: Result<AssembledGeneration, AssembleError>,
    ) {
        if epoch != self.epoch {
            self.emit(
                Some(request_id),
                FrontendUpdateKind::CommandRejected {
                    reason: CommandReject::NotAccepting,
                },
            );
            return;
        }
        match result {
            Ok(assembled) => {
                if let Some(next) = self.generation.install_success(request_id, assembled) {
                    // Update the per-session cache so the next submit uses the
                    // new generation; also keeps the global cell as a fallback.
                    if let Some(session_id) = self.snapshot.current_session.clone() {
                        self.session_generations.put(session_id, next.clone());
                    }
                    self.emit(
                        Some(request_id),
                        FrontendUpdateKind::GenerationInstalled {
                            display: mapping::frontend_generation(&next),
                        },
                    );
                } else {
                    self.feed.cancel_request(request_id);
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
                } else {
                    self.feed.cancel_request(request_id);
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
                    reason: CommandReject::CancelRejected {
                        message: format!("{other:?}"),
                    },
                },
            ),
        }
    }

    fn spawn_install(
        &mut self,
        request_id: FrontendRequestId,
        name: String,
        effort: Option<FrontendReasoningEffort>,
    ) {
        let assembler = self.assembler.clone();
        let epoch = self.epoch;
        let effort = effort.map(mapping::reasoning_effort);
        self.spawn_work(request_id, async move {
            let result = assembler
                .assemble(AssembleRequest {
                    name: name.clone(),
                    effort,
                })
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
            generation: self.session_generation(&session_id),
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
