//! The public runtime entry point: start the coordinator actor.

use crate::coordinator::{Coordinator, empty_snapshot};
use crate::epoch::{EpochShared, SuperviseEpoch, supervise_epoch};
use crate::shutdown::ShutdownRequest;
use crate::{
    ChannelBounds, IdSource, RuntimeEpoch, RuntimeEventReceiver, RuntimeHandle, SequentialIdSource,
    StartError,
};
use philo_session as session;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{mpsc, watch};

/// Construction-only type. After [`AgentRuntime::start`] the caller holds
/// [`RuntimeParts`].
pub struct AgentRuntime;

/// One-shot runtime construction result. The event receiver can be taken once.
pub struct RuntimeParts {
    pub handle: RuntimeHandle,
    pub events: RuntimeEventReceiver,
}

/// Dependencies frozen for one runtime epoch. Model/tools/config arrive
/// per-submit on [`crate::OperationSpec::generation`].
pub struct RuntimeDeps {
    pub sessions: Arc<dyn session::SessionStore>,
    pub ids: Arc<dyn IdSource>,
    pub bounds: ChannelBounds,
}

impl Default for RuntimeDeps {
    fn default() -> Self {
        Self {
            sessions: Arc::new(session::MemorySessionStore::new()),
            ids: Arc::new(SequentialIdSource::new()),
            bounds: ChannelBounds::default(),
        }
    }
}

static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

impl AgentRuntime {
    /// Starts a self-driven coordinator on the current Tokio runtime.
    ///
    /// The event receiver must be handed to AgentService immediately. Closing
    /// it is a host failure, not an invitation to buffer events in memory.
    pub fn start(deps: RuntimeDeps) -> Result<RuntimeParts, StartError> {
        let _runtime =
            tokio::runtime::Handle::try_current().map_err(|_| StartError::RuntimeUnavailable {
                message: "AgentRuntime::start requires a Tokio runtime".to_owned(),
            })?;
        let bounds = deps.bounds.validate()?;
        let epoch = RuntimeEpoch::new(format!(
            "epoch-{}",
            NEXT_EPOCH.fetch_add(1, Ordering::Relaxed)
        ));
        let (command_tx, command_rx) = mpsc::channel(bounds.command_cap);
        let (control_tx, control_rx) = mpsc::channel(bounds.control_cap);
        let (event_tx, event_rx) = mpsc::channel(bounds.event_cap);
        let (snapshot_tx, snapshot_rx) = watch::channel(empty_snapshot(epoch.clone()));
        let (shutdown_tx, shutdown_rx) = watch::channel(None::<ShutdownRequest>);
        let (completion_tx, completion_rx) =
            watch::channel(None::<crate::shutdown::ShutdownOutcome>);
        let shared = EpochShared::new(bounds.queue_max, bounds.reliable_staging_cap);
        let join = Coordinator::spawn(
            epoch.clone(),
            deps.sessions,
            deps.ids,
            bounds,
            command_rx,
            control_rx,
            event_tx.clone(),
            snapshot_tx.clone(),
            shutdown_rx.clone(),
            shared.clone(),
        );
        tokio::spawn(supervise_epoch(SuperviseEpoch {
            epoch,
            coordinator: join,
            shared,
            event_tx,
            snapshot_tx,
            shutdown_rx,
            completion_tx,
        }));
        Ok(RuntimeParts {
            handle: RuntimeHandle {
                command_tx,
                control_tx,
                snapshot_rx,
                shutdown_tx,
                completion_rx,
            },
            events: RuntimeEventReceiver { events: event_rx },
        })
    }
}
