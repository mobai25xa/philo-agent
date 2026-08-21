//! Public service handle. The actor lives in [`crate::actor`].

use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::{mpsc, watch};
use tokio::task::{AbortHandle, JoinHandle};

use crate::actor::AgentServiceActor;
use crate::bounds::{
    FRONTEND_COMMAND_CAP, FRONTEND_CONTROL_CAP, FRONTEND_CRITICAL_CAP, FRONTEND_SNAPSHOT_CAP,
    FRONTEND_SUPERVISOR_CAP, FRONTEND_UPDATE_CAP,
};
use crate::confirmation::{ConfirmationGate, ConfirmationMap, gate_pair};
use crate::error::CommandDispatch;
use crate::frontend::command::FrontendCommand;
use crate::frontend::lease::{
    AttachError, DetachError, DetachReport, FrontendLease, SupervisorCommand,
};
use crate::frontend::supervisor::{SupervisorEnvelope, SupervisorReply, exchange_supervisor};
use crate::frontend::update::FrontendUpdate;
use crate::frontend::{CommandEnvelope, FrontendClient, FrontendFeed, FrontendLanes, ReplyCredits};
use crate::generation::GenerationAssembler;
use crate::ids::{FrontendInstanceId, FrontendRequestId, RequestIdSource};
use crate::runtime_api::{RuntimeEvents, RuntimePort};
use philo_agent_runtime::RuntimeGeneration;
use philo_session::SessionStore;

/// Dependencies required to start the application service.
pub struct ServiceDeps<R, S> {
    /// Cloneable runtime control plane.
    pub runtime: R,
    /// Bounded runtime subscription. The actor consumes it continuously.
    pub subscription: S,
    /// Durable session store. Queried on snapshot/load/preview; never cached whole.
    pub sessions: Arc<dyn SessionStore>,
    /// Background generation assembler (CLI injects the real one in Wave 2).
    pub assembler: Arc<dyn GenerationAssembler>,
    /// Generation installed at process start.
    pub initial_generation: Arc<RuntimeGeneration>,
}

/// Supervisor handle for the service actor. Dropping it does not cancel Runtime.
pub struct AgentService {
    join: Mutex<Option<JoinHandle<()>>>,
    abort: AbortHandle,
    confirmations: ConfirmationGate,
    control_tx: mpsc::Sender<CommandEnvelope>,
    critical_tx: mpsc::WeakSender<FrontendUpdate>,
    credits: ReplyCredits,
    request_ids: Arc<RequestIdSource>,
    supervisor_tx: mpsc::Sender<SupervisorEnvelope>,
}

impl AgentService {
    /// Gate used by approval decorators. Auto-denies on detach/settle/shutdown.
    pub fn confirmation_gate(&self) -> ConfirmationGate {
        self.confirmations.clone()
    }

    /// Registers one frontend instance and waits for a confirmed lease.
    pub async fn attach_frontend(
        &self,
        frontend_id: FrontendInstanceId,
        deadline: Instant,
    ) -> Result<FrontendLease, AttachError> {
        match exchange_supervisor(
            &self.supervisor_tx,
            SupervisorCommand::AttachFrontend { id: frontend_id },
            deadline,
        )
        .await
        {
            Ok(SupervisorReply::Attach(result)) => result,
            Ok(SupervisorReply::Detach(_) | SupervisorReply::Shutdown) => {
                Err(AttachError::ServiceGone)
            }
            Err(error) => Err(error.into_attach()),
        }
    }

    /// Releases a previously issued lease and waits for confirmation.
    pub async fn detach_frontend(
        &self,
        lease: FrontendLease,
        deadline: Instant,
    ) -> Result<DetachReport, DetachError> {
        match exchange_supervisor(
            &self.supervisor_tx,
            SupervisorCommand::DetachFrontend { lease },
            deadline,
        )
        .await
        {
            Ok(SupervisorReply::Detach(result)) => result,
            Ok(SupervisorReply::Attach(_) | SupervisorReply::Shutdown) => {
                Err(DetachError::ServiceGone)
            }
            Err(error) => Err(error.into_detach()),
        }
    }

    /// Supervisor-lane host shutdown: detach-equivalent cleanup, then drain Runtime.
    /// Distinct from [`philo_agent_runtime::RuntimePort::shutdown`].
    pub async fn shutdown_from_supervisor(
        &self,
        reason: impl Into<String>,
        deadline: Instant,
    ) -> Result<(), AttachError> {
        match exchange_supervisor(
            &self.supervisor_tx,
            SupervisorCommand::Shutdown {
                reason: reason.into(),
            },
            deadline,
        )
        .await
        {
            Ok(SupervisorReply::Shutdown) => Ok(()),
            Ok(_) => Err(AttachError::ServiceGone),
            Err(error) => Err(error.into_attach()),
        }
    }

    /// Enqueues an explicit shutdown. Does not abort in-flight Runtime work
    /// beyond what [`philo_agent_runtime::ShutdownMode::Drain`] requests.
    pub fn request_shutdown(&self) -> CommandDispatch<FrontendRequestId> {
        let request_id = self.request_ids.next();
        let Some(critical_tx) = self.critical_tx.upgrade() else {
            return CommandDispatch::Disconnected {
                lane: "frontend-control",
            };
        };
        if !self.credits.reserve(&critical_tx, request_id, 1) {
            return CommandDispatch::Backpressured;
        }
        match self.control_tx.try_send(CommandEnvelope {
            request_id,
            command: FrontendCommand::ShutdownRequested,
        }) {
            Ok(()) => CommandDispatch::Enqueued(request_id),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.credits.cancel(request_id);
                CommandDispatch::Backpressured
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.credits.cancel(request_id);
                CommandDispatch::Disconnected {
                    lane: "frontend-control",
                }
            }
        }
    }

    /// Waits for the actor task to exit. Does not depend on an internal reply lane.
    pub async fn join(self) {
        self.wait_stopped().await;
    }

    /// Waits for the actor to exit without consuming supervisor access.
    pub async fn wait_stopped(&self) {
        let handle = self
            .join
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    /// Aborts the actor task. The process supervisor uses this after a deadline.
    pub fn abort_actor(&self) {
        self.abort.abort();
    }
}

pub(crate) struct StartOptions {
    pub command_hold: Option<watch::Receiver<bool>>,
}

/// Starts the service actor on the current Tokio runtime.
pub fn start<R, S>(deps: ServiceDeps<R, S>) -> (AgentService, FrontendClient)
where
    R: RuntimePort,
    S: RuntimeEvents + 'static,
{
    start_inner(deps, StartOptions { command_hold: None })
}

pub(crate) fn start_inner<R, S>(
    deps: ServiceDeps<R, S>,
    options: StartOptions,
) -> (AgentService, FrontendClient)
where
    R: RuntimePort,
    S: RuntimeEvents + 'static,
{
    let (command_tx, command_rx) = mpsc::channel(FRONTEND_COMMAND_CAP);
    let (control_tx, control_rx) = mpsc::channel(FRONTEND_CONTROL_CAP);
    let (snapshot_tx, snapshot_rx) = mpsc::channel(FRONTEND_SNAPSHOT_CAP);
    let (update_tx, update_rx) = mpsc::channel(FRONTEND_UPDATE_CAP);
    let (critical_tx, critical_rx) = mpsc::channel(FRONTEND_CRITICAL_CAP);
    let critical_weak = critical_tx.downgrade();
    let credits = ReplyCredits::new();
    let request_ids = Arc::new(RequestIdSource::new());
    let (supervisor_tx, supervisor_rx) = mpsc::channel(FRONTEND_SUPERVISOR_CAP);
    let (confirmations, confirm_rx) = gate_pair();

    let client = FrontendClient::new(
        FrontendLanes {
            command: command_tx.clone(),
            control: control_tx.clone(),
            snapshot: snapshot_tx,
            critical: critical_weak.clone(),
        },
        update_rx,
        critical_rx,
        credits.clone(),
        request_ids.clone(),
    );
    let actor = AgentServiceActor::new(
        deps.runtime,
        deps.subscription,
        deps.sessions,
        deps.assembler,
        deps.initial_generation,
        ConfirmationMap::new(),
        FrontendFeed::new(update_tx, critical_tx, credits.clone()),
    );
    let join = tokio::spawn(actor.run(
        command_rx,
        control_rx,
        snapshot_rx,
        confirm_rx,
        supervisor_rx,
        options.command_hold,
    ));
    let abort = join.abort_handle();
    (
        AgentService {
            join: Mutex::new(Some(join)),
            abort,
            confirmations,
            control_tx,
            critical_tx: critical_weak,
            credits,
            request_ids,
            supervisor_tx,
        },
        client,
    )
}
