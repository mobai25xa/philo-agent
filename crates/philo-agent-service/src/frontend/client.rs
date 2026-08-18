//! Frontend client. Bounded channels stay private.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, mpsc};

use crate::error::{CommandDispatch, RecvOutcome};
use crate::frontend::command::FrontendCommand;
use crate::frontend::update::FrontendUpdate;
use crate::ids::{FrontendRequestId, FrontendRevision, RequestIdSource};

pub(crate) struct CommandEnvelope {
    pub request_id: FrontendRequestId,
    pub command: FrontendCommand,
}

/// TUI-facing handle. Clone shares the same lanes and the same update receiver.
#[derive(Clone)]
pub struct FrontendClient {
    command_tx: mpsc::Sender<CommandEnvelope>,
    control_tx: mpsc::Sender<CommandEnvelope>,
    snapshot_tx: mpsc::Sender<CommandEnvelope>,
    update_rx: Arc<Mutex<mpsc::Receiver<FrontendUpdate>>>,
    ids: Arc<RequestIdSource>,
}

impl FrontendClient {
    pub(crate) fn new(
        command_tx: mpsc::Sender<CommandEnvelope>,
        control_tx: mpsc::Sender<CommandEnvelope>,
        snapshot_tx: mpsc::Sender<CommandEnvelope>,
        update_rx: mpsc::Receiver<FrontendUpdate>,
    ) -> Self {
        Self {
            command_tx,
            control_tx,
            snapshot_tx,
            update_rx: Arc::new(Mutex::new(update_rx)),
            ids: Arc::new(RequestIdSource::new()),
        }
    }

    /// Non-blocking command submit. Never waits for the service actor.
    pub fn try_command(&self, command: FrontendCommand) -> CommandDispatch<FrontendRequestId> {
        let request_id = self.ids.next();
        let (tx, lane) = if command.is_snapshot() {
            (&self.snapshot_tx, "frontend-snapshot")
        } else if command.is_control() {
            (&self.control_tx, "frontend-control")
        } else {
            (&self.command_tx, "frontend-command")
        };
        match tx.try_send(CommandEnvelope {
            request_id,
            command,
        }) {
            Ok(()) => CommandDispatch::Enqueued(request_id),
            Err(mpsc::error::TrySendError::Full(_)) => CommandDispatch::Backpressured,
            Err(mpsc::error::TrySendError::Closed(_)) => CommandDispatch::Disconnected { lane },
        }
    }

    /// Requests a composed snapshot. Uses the reserved recovery lane.
    pub fn request_snapshot(
        &self,
        known_revision: FrontendRevision,
    ) -> CommandDispatch<FrontendRequestId> {
        self.try_command(FrontendCommand::RequestSnapshot { known_revision })
    }

    /// Async alias of [`Self::request_snapshot`].
    pub async fn request_snapshot_async(
        &self,
        known_revision: FrontendRevision,
    ) -> CommandDispatch<FrontendRequestId> {
        self.request_snapshot(known_revision)
    }

    /// Non-blocking receive of an already-queued update.
    pub fn try_recv(&self) -> RecvOutcome {
        let Ok(mut rx) = self.update_rx.try_lock() else {
            return RecvOutcome::Timeout;
        };
        match rx.try_recv() {
            Ok(update) => RecvOutcome::Update(update),
            Err(mpsc::error::TryRecvError::Empty) => RecvOutcome::Timeout,
            Err(mpsc::error::TryRecvError::Disconnected) => RecvOutcome::Disconnected,
        }
    }

    /// Blocks until an update, the deadline, or disconnect.
    ///
    /// Requires a Tokio runtime. Prefer [`Self::recv_until_async`] from async code.
    pub fn recv_until(&self, deadline: Instant) -> RecvOutcome {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                tokio::task::block_in_place(|| handle.block_on(self.recv_until_async(deadline)))
            }
            Err(_) => self.recv_until_spin(deadline),
        }
    }

    /// Async receive with the same deadline/disconnect semantics as [`Self::recv_until`].
    pub async fn recv_until_async(&self, deadline: Instant) -> RecvOutcome {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return RecvOutcome::Timeout;
        }
        let mut rx = self.update_rx.lock().await;
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(update)) => RecvOutcome::Update(update),
            Ok(None) => RecvOutcome::Disconnected,
            Err(_) => RecvOutcome::Timeout,
        }
    }

    fn recv_until_spin(&self, deadline: Instant) -> RecvOutcome {
        loop {
            if let Ok(mut rx) = self.update_rx.try_lock() {
                match rx.try_recv() {
                    Ok(update) => return RecvOutcome::Update(update),
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        return RecvOutcome::Disconnected;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => {}
                }
            }
            if Instant::now() >= deadline {
                return RecvOutcome::Timeout;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bounds::FRONTEND_COMMAND_CAP;
    use crate::error::CommandDispatch;
    use crate::frontend::command::FrontendCommand;
    use crate::ids::FrontendRevision;

    #[test]
    fn try_command_returns_backpressured_when_lane_is_full() {
        let (command_tx, _command_rx) = mpsc::channel(FRONTEND_COMMAND_CAP);
        let (control_tx, _control_rx) = mpsc::channel(1);
        let (snapshot_tx, _snapshot_rx) = mpsc::channel(1);
        let (_update_tx, update_rx) = mpsc::channel(1);
        let client = FrontendClient::new(command_tx, control_tx, snapshot_tx, update_rx);
        let mut enqueued = 0;
        let mut backpressured = 0;
        for _ in 0..FRONTEND_COMMAND_CAP + 4 {
            match client.try_command(FrontendCommand::ListSessions) {
                CommandDispatch::Enqueued(_) => enqueued += 1,
                CommandDispatch::Backpressured => backpressured += 1,
                CommandDispatch::Disconnected { lane } => panic!("disconnected: {lane}"),
            }
        }
        assert_eq!(enqueued, FRONTEND_COMMAND_CAP);
        assert!(backpressured > 0);
    }

    #[test]
    fn request_snapshot_returns_backpressured_when_lane_is_full() {
        let (command_tx, _command_rx) = mpsc::channel(1);
        let (control_tx, _control_rx) = mpsc::channel(1);
        let (snapshot_tx, _snapshot_rx) = mpsc::channel(1);
        let (_update_tx, update_rx) = mpsc::channel(1);
        let client = FrontendClient::new(command_tx, control_tx, snapshot_tx, update_rx);
        assert!(matches!(
            client.request_snapshot(FrontendRevision::ZERO),
            CommandDispatch::Enqueued(_)
        ));
        assert!(matches!(
            client.request_snapshot(FrontendRevision::ZERO),
            CommandDispatch::Backpressured
        ));
    }
}
