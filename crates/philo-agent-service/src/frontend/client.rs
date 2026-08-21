//! Frontend client. Bounded channels stay private.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, mpsc};

use crate::error::{CommandDispatch, RecvOutcome};
use crate::frontend::command::FrontendCommand;
use crate::frontend::feed::ReplyCredits;
use crate::frontend::update::FrontendUpdate;
use crate::ids::{FrontendRequestId, FrontendRevision, RequestIdSource};

pub(crate) struct CommandEnvelope {
    pub request_id: FrontendRequestId,
    pub command: FrontendCommand,
}

/// The three command lanes plus the weak critical sender, grouped so the
/// client constructor stays within a readable argument count.
pub(crate) struct FrontendLanes {
    pub(crate) command: mpsc::Sender<CommandEnvelope>,
    pub(crate) control: mpsc::Sender<CommandEnvelope>,
    pub(crate) snapshot: mpsc::Sender<CommandEnvelope>,
    pub(crate) critical: mpsc::WeakSender<FrontendUpdate>,
}

/// TUI-facing handle. Clone shares the same lanes and the same update receiver.
#[derive(Clone)]
pub struct FrontendClient {
    command_tx: mpsc::Sender<CommandEnvelope>,
    control_tx: mpsc::Sender<CommandEnvelope>,
    snapshot_tx: mpsc::Sender<CommandEnvelope>,
    critical_tx: mpsc::WeakSender<FrontendUpdate>,
    credits: ReplyCredits,
    update_rx: Arc<Mutex<FrontendUpdateReceiver>>,
    ids: Arc<RequestIdSource>,
}

impl FrontendClient {
    pub(crate) fn new(
        lanes: FrontendLanes,
        update_rx: mpsc::Receiver<FrontendUpdate>,
        critical_rx: mpsc::Receiver<FrontendUpdate>,
        credits: ReplyCredits,
        ids: Arc<RequestIdSource>,
    ) -> Self {
        Self {
            command_tx: lanes.command,
            control_tx: lanes.control,
            snapshot_tx: lanes.snapshot,
            critical_tx: lanes.critical,
            credits,
            update_rx: Arc::new(Mutex::new(FrontendUpdateReceiver::new(
                update_rx,
                critical_rx,
            ))),
            ids,
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
        let expected_replies = if matches!(&command, FrontendCommand::PreviewSession { .. }) {
            2
        } else {
            1
        };
        let Some(critical_tx) = self.critical_tx.upgrade() else {
            return CommandDispatch::Disconnected { lane };
        };
        if !self
            .credits
            .reserve(&critical_tx, request_id, expected_replies)
        {
            return CommandDispatch::Backpressured;
        }
        match tx.try_send(CommandEnvelope {
            request_id,
            command,
        }) {
            Ok(()) => CommandDispatch::Enqueued(request_id),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.credits.cancel(request_id);
                CommandDispatch::Backpressured
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.credits.cancel(request_id);
                CommandDispatch::Disconnected { lane }
            }
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
            Some(update) => RecvOutcome::Update(update),
            None if rx.is_disconnected() => RecvOutcome::Disconnected,
            None => RecvOutcome::Timeout,
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
                if let Some(update) = rx.try_recv() {
                    return RecvOutcome::Update(update);
                }
                if rx.is_disconnected() {
                    return RecvOutcome::Disconnected;
                }
            }
            if Instant::now() >= deadline {
                return RecvOutcome::Timeout;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

struct FrontendUpdateReceiver {
    normal: mpsc::Receiver<FrontendUpdate>,
    critical: mpsc::Receiver<FrontendUpdate>,
    normal_buffer: Option<FrontendUpdate>,
    critical_buffer: Option<FrontendUpdate>,
    normal_closed: bool,
    critical_closed: bool,
}

impl FrontendUpdateReceiver {
    fn new(
        normal: mpsc::Receiver<FrontendUpdate>,
        critical: mpsc::Receiver<FrontendUpdate>,
    ) -> Self {
        Self {
            normal,
            critical,
            normal_buffer: None,
            critical_buffer: None,
            normal_closed: false,
            critical_closed: false,
        }
    }

    fn try_recv(&mut self) -> Option<FrontendUpdate> {
        self.fill_buffers();
        self.pop_next()
    }

    fn fill_buffers(&mut self) {
        if self.normal_buffer.is_none() && !self.normal_closed {
            match self.normal.try_recv() {
                Ok(update) => self.normal_buffer = Some(update),
                Err(mpsc::error::TryRecvError::Disconnected) => self.normal_closed = true,
                Err(mpsc::error::TryRecvError::Empty) => {}
            }
        }
        if self.critical_buffer.is_none() && !self.critical_closed {
            match self.critical.try_recv() {
                Ok(update) => self.critical_buffer = Some(update),
                Err(mpsc::error::TryRecvError::Disconnected) => self.critical_closed = true,
                Err(mpsc::error::TryRecvError::Empty) => {}
            }
        }
    }

    fn pop_next(&mut self) -> Option<FrontendUpdate> {
        match (&self.normal_buffer, &self.critical_buffer) {
            (Some(normal), Some(critical)) if normal.revision <= critical.revision => {
                self.normal_buffer.take()
            }
            (Some(_), Some(_)) => self.critical_buffer.take(),
            (Some(_), None) => self.normal_buffer.take(),
            (None, Some(_)) => self.critical_buffer.take(),
            (None, None) => None,
        }
    }

    fn is_disconnected(&self) -> bool {
        self.normal_closed
            && self.critical_closed
            && self.normal_buffer.is_none()
            && self.critical_buffer.is_none()
    }

    async fn recv(&mut self) -> Option<FrontendUpdate> {
        loop {
            if let Some(update) = self.try_recv() {
                return Some(update);
            }
            match (self.normal_closed, self.critical_closed) {
                (true, true) => return None,
                (true, false) => match self.critical.recv().await {
                    Some(update) => self.critical_buffer = Some(update),
                    None => self.critical_closed = true,
                },
                (false, true) => match self.normal.recv().await {
                    Some(update) => self.normal_buffer = Some(update),
                    None => self.normal_closed = true,
                },
                (false, false) => {
                    tokio::select! {
                        update = self.critical.recv() => match update {
                            Some(update) => self.critical_buffer = Some(update),
                            None => self.critical_closed = true,
                        },
                        update = self.normal.recv() => match update {
                            Some(update) => self.normal_buffer = Some(update),
                            None => self.normal_closed = true,
                        },
                    }
                }
            }
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

    fn update(revision: u64) -> FrontendUpdate {
        FrontendUpdate::new(
            crate::ids::FrontendEpoch::INITIAL,
            FrontendRevision::new(revision),
            None,
            crate::frontend::update::FrontendUpdateKind::CommandAccepted,
        )
    }

    #[test]
    fn merged_update_lanes_preserve_revision_order() {
        let (normal_tx, normal_rx) = mpsc::channel(2);
        let (critical_tx, critical_rx) = mpsc::channel(2);
        normal_tx.try_send(update(1)).unwrap();
        critical_tx.try_send(update(2)).unwrap();
        let mut receiver = FrontendUpdateReceiver::new(normal_rx, critical_rx);
        assert_eq!(
            receiver.try_recv().unwrap().revision,
            FrontendRevision::new(1)
        );
        assert_eq!(
            receiver.try_recv().unwrap().revision,
            FrontendRevision::new(2)
        );
    }

    #[test]
    fn try_command_returns_backpressured_when_lane_is_full() {
        let (command_tx, _command_rx) = mpsc::channel(FRONTEND_COMMAND_CAP);
        let (control_tx, _control_rx) = mpsc::channel(1);
        let (snapshot_tx, _snapshot_rx) = mpsc::channel(1);
        let (_update_tx, update_rx) = mpsc::channel(1);
        let (critical_tx, critical_rx) = mpsc::channel(FRONTEND_COMMAND_CAP + 4);
        let client = FrontendClient::new(
            FrontendLanes {
                command: command_tx,
                control: control_tx,
                snapshot: snapshot_tx,
                critical: critical_tx.downgrade(),
            },
            update_rx,
            critical_rx,
            ReplyCredits::new(),
            Arc::new(RequestIdSource::new()),
        );
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
        let (critical_tx, critical_rx) = mpsc::channel(1);
        let client = FrontendClient::new(
            FrontendLanes {
                command: command_tx,
                control: control_tx,
                snapshot: snapshot_tx,
                critical: critical_tx.downgrade(),
            },
            update_rx,
            critical_rx,
            ReplyCredits::new(),
            Arc::new(RequestIdSource::new()),
        );
        assert!(matches!(
            client.request_snapshot(FrontendRevision::ZERO),
            CommandDispatch::Enqueued(_)
        ));
        assert!(matches!(
            client.request_snapshot(FrontendRevision::ZERO),
            CommandDispatch::Backpressured
        ));
    }

    #[tokio::test]
    async fn async_receive_reads_the_critical_lane() {
        let (command_tx, _command_rx) = mpsc::channel(1);
        let (control_tx, _control_rx) = mpsc::channel(1);
        let (snapshot_tx, _snapshot_rx) = mpsc::channel(1);
        let (_update_tx, update_rx) = mpsc::channel(1);
        let (critical_tx, critical_rx) = mpsc::channel(1);
        let client = FrontendClient::new(
            FrontendLanes {
                command: command_tx,
                control: control_tx,
                snapshot: snapshot_tx,
                critical: critical_tx.downgrade(),
            },
            update_rx,
            critical_rx,
            ReplyCredits::new(),
            Arc::new(RequestIdSource::new()),
        );
        critical_tx.try_send(update(1)).unwrap();
        assert!(matches!(
            client
                .recv_until_async(Instant::now() + std::time::Duration::from_secs(1))
                .await,
            RecvOutcome::Update(update) if update.revision == FrontendRevision::new(1)
        ));
    }

    #[test]
    fn client_observes_update_disconnect_without_owning_a_sender() {
        let (command_tx, _command_rx) = mpsc::channel(1);
        let (control_tx, _control_rx) = mpsc::channel(1);
        let (snapshot_tx, _snapshot_rx) = mpsc::channel(1);
        let (update_tx, update_rx) = mpsc::channel(1);
        let (critical_tx, critical_rx) = mpsc::channel(1);
        let client = FrontendClient::new(
            FrontendLanes {
                command: command_tx,
                control: control_tx,
                snapshot: snapshot_tx,
                critical: critical_tx.downgrade(),
            },
            update_rx,
            critical_rx,
            ReplyCredits::new(),
            Arc::new(RequestIdSource::new()),
        );
        drop(update_tx);
        drop(critical_tx);

        assert!(matches!(client.try_recv(), RecvOutcome::Disconnected));
        assert!(matches!(
            client.try_command(FrontendCommand::ListSessions),
            CommandDispatch::Disconnected { .. }
        ));
    }
}
