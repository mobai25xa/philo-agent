//! Public service handle. The actor lives in [`crate::actor`].

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::actor::AgentServiceActor;
use crate::bounds::{
    FRONTEND_COMMAND_CAP, FRONTEND_CONTROL_CAP, FRONTEND_SNAPSHOT_CAP, FRONTEND_UPDATE_CAP,
};
use crate::confirmation::{ConfirmationGate, ConfirmationMap, gate_pair};
use crate::error::CommandSubmitResult;
use crate::frontend::command::FrontendCommand;
use crate::frontend::{CommandEnvelope, FrontendClient, FrontendFeed};
use crate::generation::GenerationAssembler;
use crate::ids::FrontendRequestId;
use crate::runtime_api::{RuntimeEvents, RuntimePort};
use philo_agent_runtime::RuntimeGeneration;
use philo_session::SessionStore;
use std::sync::Arc;

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
    join: JoinHandle<()>,
    confirmations: ConfirmationGate,
    control_tx: mpsc::Sender<CommandEnvelope>,
}

impl AgentService {
    /// Gate used by approval decorators. Auto-denies on detach/settle/shutdown.
    pub fn confirmation_gate(&self) -> ConfirmationGate {
        self.confirmations.clone()
    }

    /// Enqueues an explicit shutdown. Does not abort in-flight Runtime work
    /// beyond what [`philo_agent_runtime::ShutdownMode::Drain`] requests.
    pub fn request_shutdown(&self) -> CommandSubmitResult {
        match self.control_tx.try_send(CommandEnvelope {
            request_id: FrontendRequestId::new(u64::MAX),
            command: FrontendCommand::ShutdownRequested,
        }) {
            Ok(()) => CommandSubmitResult::Accepted(FrontendRequestId::new(u64::MAX)),
            Err(mpsc::error::TrySendError::Full(_)) => CommandSubmitResult::Backpressured,
            Err(mpsc::error::TrySendError::Closed(_)) => CommandSubmitResult::Disconnected,
        }
    }

    /// Waits for the actor task to exit. Does not depend on an internal reply lane.
    pub async fn join(self) {
        let _ = self.join.await;
    }
}

/// Starts the service actor on the current Tokio runtime.
pub fn start<R, S>(deps: ServiceDeps<R, S>) -> (AgentService, FrontendClient)
where
    R: RuntimePort,
    S: RuntimeEvents + 'static,
{
    let (command_tx, command_rx) = mpsc::channel(FRONTEND_COMMAND_CAP);
    let (control_tx, control_rx) = mpsc::channel(FRONTEND_CONTROL_CAP);
    let (snapshot_tx, snapshot_rx) = mpsc::channel(FRONTEND_SNAPSHOT_CAP);
    let (update_tx, update_rx) = mpsc::channel(FRONTEND_UPDATE_CAP);
    let (confirmations, confirm_rx) = gate_pair();

    let client = FrontendClient::new(command_tx, control_tx.clone(), snapshot_tx, update_rx);
    let actor = AgentServiceActor::new(
        deps.runtime,
        deps.subscription,
        deps.sessions,
        deps.assembler,
        deps.initial_generation,
        ConfirmationMap::new(),
        FrontendFeed::new(update_tx),
    );
    let join = tokio::spawn(actor.run(command_rx, control_rx, snapshot_rx, confirm_rx));
    (
        AgentService {
            join,
            confirmations,
            control_tx,
        },
        client,
    )
}
