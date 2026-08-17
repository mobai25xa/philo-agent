//! The public runtime entry point: start the coordinator actor.

use crate::coordinator::{Coordinator, empty_snapshot};
use crate::epoch::{EpochShared, SuperviseEpoch, supervise_epoch};
use crate::{
    ChannelBounds, IdSource, RuntimeEpoch, RuntimeHandle, RuntimeSubscription, SequentialIdSource,
    StartError,
};
use philo_session as session;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{mpsc, watch};

/// Construction-only type. After [`AgentRuntime::start`] the caller holds
/// [`RuntimeHandle`] and [`RuntimeSubscription`].
pub struct AgentRuntime;

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
    /// Operations progress after submit even if the subscription is idle.
    pub fn start(deps: RuntimeDeps) -> Result<(RuntimeHandle, RuntimeSubscription), StartError> {
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
        let shared = EpochShared::new(bounds.queue_max);
        let join = Coordinator::spawn(
            epoch.clone(),
            deps.sessions,
            deps.ids,
            bounds,
            command_rx,
            control_rx,
            event_tx.clone(),
            snapshot_tx.clone(),
            shared.clone(),
        );
        tokio::spawn(supervise_epoch(SuperviseEpoch {
            epoch,
            coordinator: join,
            shared,
            event_tx,
            snapshot_tx,
        }));
        Ok((
            RuntimeHandle {
                command_tx,
                control_tx,
                snapshot_rx,
            },
            RuntimeSubscription { events: event_rx },
        ))
    }
}
