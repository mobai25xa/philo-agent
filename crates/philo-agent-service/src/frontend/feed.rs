//! Bounded frontend feed. Ordinary overflow marks `needs_resync`; request
//! terminals and recovery facts use a reserved lane so they cannot be
//! displaced by a burst of live deltas.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::frontend::update::{FrontendUpdate, FrontendUpdateKind};
use crate::ids::{FrontendEpoch, FrontendRequestId, FrontendRevision};

/// Why the feed could not accept an update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedPush {
    /// Update is on the lane.
    Sent,
    /// Lane is in resync; the update was dropped.
    Dropped,
    /// A critical update could not be queued without violating its bound.
    Backpressured,
    /// Receiver is gone.
    Disconnected,
}

/// Error raised when a consumer must snapshot before applying more updates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResyncRequired {
    /// Service revision to request.
    pub latest_revision: FrontendRevision,
}

/// Reservations for request-scoped terminal updates.
///
/// The permits are owned by the queued update until the frontend receives it,
/// so a request cannot be admitted after the critical lane has run out of
/// response capacity.
#[derive(Clone, Default)]
pub(crate) struct ReplyCredits {
    permits: Arc<Mutex<HashMap<FrontendRequestId, VecDeque<mpsc::OwnedPermit<FrontendUpdate>>>>>,
    closed: Arc<AtomicBool>,
}

impl ReplyCredits {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn reserve(
        &self,
        tx: &mpsc::Sender<FrontendUpdate>,
        request_id: FrontendRequestId,
        count: usize,
    ) -> bool {
        if count == 0 {
            return true;
        }
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        let mut reserved = VecDeque::with_capacity(count);
        for _ in 0..count {
            match tx.clone().try_reserve_owned() {
                Ok(permit) => reserved.push_back(permit),
                Err(_) => return false,
            }
        }
        let mut permits = self
            .permits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.closed.load(Ordering::Acquire) || permits.contains_key(&request_id) {
            return false;
        }
        permits.insert(request_id, reserved);
        true
    }

    pub(crate) fn take(
        &self,
        request_id: FrontendRequestId,
    ) -> Option<mpsc::OwnedPermit<FrontendUpdate>> {
        let mut permits = self
            .permits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let queue = permits.get_mut(&request_id)?;
        let permit = queue.pop_front();
        if queue.is_empty() {
            permits.remove(&request_id);
        }
        permit
    }

    /// Drops any credits left by a superseded or abandoned request.
    pub(crate) fn cancel(&self, request_id: FrontendRequestId) {
        self.permits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&request_id);
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.permits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

impl Drop for FrontendFeed {
    fn drop(&mut self) {
        self.credits.close();
    }
}

/// Bounded sender side of the frontend update lane.
pub(crate) struct FrontendFeed {
    tx: mpsc::Sender<FrontendUpdate>,
    critical_tx: mpsc::Sender<FrontendUpdate>,
    credits: ReplyCredits,
    pending_snapshot: Option<FrontendUpdate>,
    needs_resync: bool,
    awaiting_snapshot: bool,
    disconnected: bool,
}

impl FrontendFeed {
    pub(crate) fn new(
        tx: mpsc::Sender<FrontendUpdate>,
        critical_tx: mpsc::Sender<FrontendUpdate>,
        credits: ReplyCredits,
    ) -> Self {
        Self {
            tx,
            critical_tx,
            credits,
            pending_snapshot: None,
            needs_resync: false,
            awaiting_snapshot: false,
            disconnected: false,
        }
    }

    pub(crate) fn pending_resync(&self) -> bool {
        self.needs_resync && !self.disconnected && !self.awaiting_snapshot
    }

    pub(crate) fn pending_critical(&self) -> bool {
        self.pending_snapshot.is_some() && !self.disconnected
    }

    pub(crate) fn is_resyncing(&self) -> bool {
        (self.needs_resync || self.awaiting_snapshot) && !self.disconnected
    }

    #[cfg(test)]
    pub(crate) fn needs_resync(&self) -> bool {
        self.needs_resync || self.awaiting_snapshot
    }

    #[cfg(test)]
    pub(crate) fn is_disconnected(&self) -> bool {
        self.disconnected
    }

    /// Mark the feed dirty so the next flush sends `ResyncRequired`.
    pub(crate) fn force_resync(&mut self) {
        if !self.disconnected && !self.awaiting_snapshot {
            self.needs_resync = true;
        }
    }

    /// Attempts to emit `update`. Never waits for the frontend.
    ///
    /// Critical facts go through the reserved lane during resync so a
    /// submit/cancel cannot hang without a terminal result.
    pub(crate) fn push(&mut self, update: FrontendUpdate) -> FeedPush {
        if self.disconnected {
            return FeedPush::Disconnected;
        }
        let critical = is_critical(&update);
        if self.awaiting_snapshot && !critical {
            return FeedPush::Dropped;
        }
        if self.needs_resync && !critical {
            self.try_resync(update.epoch, update.revision);
            return FeedPush::Dropped;
        }
        let tx = if critical {
            &self.critical_tx
        } else {
            &self.tx
        };
        if critical
            && let Some(request_id) = update.request_id
            && let Some(permit) = self.credits.take(request_id)
        {
            permit.send(update);
            return FeedPush::Sent;
        }
        match tx.try_send(update) {
            Ok(()) => FeedPush::Sent,
            Err(mpsc::error::TrySendError::Full(update)) => {
                if critical {
                    if matches!(&update.kind, FrontendUpdateKind::SnapshotReady(_))
                        && self
                            .pending_snapshot
                            .as_ref()
                            .is_none_or(|pending| pending.request_id.is_none())
                    {
                        self.pending_snapshot = Some(update);
                    }
                    FeedPush::Backpressured
                } else {
                    self.needs_resync = true;
                    FeedPush::Dropped
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.disconnected = true;
                FeedPush::Disconnected
            }
        }
    }

    /// If the feed overflowed, try to deliver a single `ResyncRequired`.
    pub(crate) fn flush_resync(&mut self, epoch: FrontendEpoch, revision: FrontendRevision) {
        if self.disconnected || self.awaiting_snapshot || !self.needs_resync {
            return;
        }
        self.try_resync(epoch, revision);
    }

    /// Retries the single recovery snapshot retained after critical backpressure.
    pub(crate) fn flush_pending_critical(&mut self) -> Option<FeedPush> {
        let update = self.pending_snapshot.take()?;
        let result = match self.critical_tx.try_send(update) {
            Ok(()) => FeedPush::Sent,
            Err(mpsc::error::TrySendError::Full(update)) => {
                self.pending_snapshot = Some(update);
                FeedPush::Backpressured
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.disconnected = true;
                FeedPush::Disconnected
            }
        };
        self.on_snapshot_ready(result);
        Some(result)
    }

    fn try_resync(&mut self, epoch: FrontendEpoch, revision: FrontendRevision) {
        let update = FrontendUpdate::new(
            epoch,
            revision,
            None,
            FrontendUpdateKind::ResyncRequired {
                latest_revision: revision,
            },
        );
        match self.critical_tx.try_send(update) {
            Ok(()) => {
                self.needs_resync = false;
                self.awaiting_snapshot = true;
            }
            Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.disconnected = true;
            }
        }
    }

    /// Resume deltas only after `SnapshotReady` reached its lane.
    pub(crate) fn on_snapshot_ready(&mut self, push: FeedPush) {
        if matches!(push, FeedPush::Sent) {
            self.needs_resync = false;
            self.awaiting_snapshot = false;
        }
    }

    pub(crate) fn cancel_request(&self, request_id: FrontendRequestId) {
        self.credits.cancel(request_id);
    }
}

fn is_critical(update: &FrontendUpdate) -> bool {
    update.request_id.is_some()
        || matches!(
            &update.kind,
            FrontendUpdateKind::SnapshotReady(_)
                | FrontendUpdateKind::ResyncRequired { .. }
                | FrontendUpdateKind::ConfirmationResolved { .. }
                | FrontendUpdateKind::OperationEvent(
                    crate::frontend::snapshot::FrontendOperationEvent::OperationSettled { .. }
                )
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::snapshot::{
        FrontendAvailability, FrontendGeneration, FrontendSnapshot, ServiceHealth,
    };
    use crate::frontend::update::FrontendUpdateKind;
    use crate::ids::FrontendEpoch;

    fn credits() -> ReplyCredits {
        ReplyCredits::new()
    }

    fn update(revision: u64) -> FrontendUpdate {
        FrontendUpdate::new(
            FrontendEpoch::INITIAL,
            FrontendRevision::new(revision),
            None,
            FrontendUpdateKind::CommandAccepted,
        )
    }

    fn snapshot_update(revision: u64) -> FrontendUpdate {
        FrontendUpdate::new(
            FrontendEpoch::INITIAL,
            FrontendRevision::new(revision),
            None,
            FrontendUpdateKind::SnapshotReady(Box::new(FrontendSnapshot {
                epoch: FrontendEpoch::INITIAL,
                revision: FrontendRevision::new(revision),
                current_session_id: None,
                durable_session_view: None,
                live: crate::live::LiveOperationSnapshot::default(),
                queued: Vec::new(),
                maintenance: None,
                availability: FrontendAvailability::Idle,
                generation: FrontendGeneration {
                    generation_id: "test".into(),
                    model_name: "test".into(),
                    reasoning_effort: None,
                    tool_names: Vec::new(),
                },
                usage: None,
                pending_confirmations: Vec::new(),
                config_notices: Vec::new(),
                health: ServiceHealth::Ok,
            })),
        )
    }

    #[test]
    fn overflow_is_bounded_and_requests_resync() {
        let (tx, mut rx) = mpsc::channel(2);
        let (critical_tx, mut critical_rx) = mpsc::channel(2);
        let mut feed = FrontendFeed::new(tx, critical_tx, credits());
        assert_eq!(feed.push(update(1)), FeedPush::Sent);
        assert_eq!(feed.push(update(2)), FeedPush::Sent);
        assert_eq!(feed.push(update(3)), FeedPush::Dropped);
        assert!(critical_rx.try_recv().is_err());
        assert!(feed.needs_resync());

        let first = rx.try_recv().unwrap();
        assert!(matches!(first.kind, FrontendUpdateKind::CommandAccepted));
        feed.flush_resync(FrontendEpoch::INITIAL, FrontendRevision::new(3));
        // Recovery uses its reserved lane even while the ordinary lane is full.
        assert!(feed.needs_resync());
        let _ = rx.try_recv().unwrap();
        let resync = critical_rx.try_recv().unwrap();
        assert!(matches!(
            resync.kind,
            FrontendUpdateKind::ResyncRequired { .. }
        ));
        assert_eq!(feed.push(update(4)), FeedPush::Dropped);
        feed.on_snapshot_ready(FeedPush::Sent);
        assert_eq!(feed.push(update(5)), FeedPush::Sent);
    }

    #[test]
    fn closed_receiver_is_disconnected_not_blocking() {
        let (tx, _rx) = mpsc::channel(1);
        let (critical_tx, critical_rx) = mpsc::channel(1);
        drop(_rx);
        drop(critical_rx);
        let mut feed = FrontendFeed::new(tx, critical_tx, credits());
        assert_eq!(feed.push(update(1)), FeedPush::Disconnected);
        assert!(feed.is_disconnected());
    }

    #[test]
    fn critical_reply_survives_full_ordinary_lane() {
        let (tx, mut rx) = mpsc::channel(1);
        let (critical_tx, mut critical_rx) = mpsc::channel(1);
        let credits = credits();
        let request_id = FrontendRequestId::new(7);
        assert!(credits.reserve(&critical_tx, request_id, 1));
        let mut feed = FrontendFeed::new(tx, critical_tx, credits);
        assert_eq!(feed.push(update(1)), FeedPush::Sent);
        let reply = FrontendUpdate::new(
            FrontendEpoch::INITIAL,
            FrontendRevision::new(2),
            Some(request_id),
            FrontendUpdateKind::CommandRejected {
                reason: crate::error::CommandReject::NotAccepting,
            },
        );
        assert_eq!(feed.push(reply), FeedPush::Sent);
        assert!(rx.try_recv().is_ok());
        assert!(critical_rx.try_recv().is_ok());
    }

    #[test]
    fn unreserved_critical_update_reports_backpressure() {
        let (tx, _rx) = mpsc::channel(1);
        let (critical_tx, _critical_rx) = mpsc::channel(1);
        let mut feed = FrontendFeed::new(tx, critical_tx, credits());
        let update = FrontendUpdate::new(
            FrontendEpoch::INITIAL,
            FrontendRevision::new(1),
            Some(FrontendRequestId::new(8)),
            FrontendUpdateKind::CommandAccepted,
        );
        assert_eq!(feed.push(update), FeedPush::Sent);
        let update = FrontendUpdate::new(
            FrontendEpoch::INITIAL,
            FrontendRevision::new(2),
            Some(FrontendRequestId::new(9)),
            FrontendUpdateKind::CommandAccepted,
        );
        assert_eq!(feed.push(update), FeedPush::Backpressured);
    }

    #[test]
    fn failed_snapshot_enqueue_keeps_resync_latched() {
        let (tx, _rx) = mpsc::channel(1);
        let (critical_tx, mut critical_rx) = mpsc::channel(1);
        let mut feed = FrontendFeed::new(tx, critical_tx, credits());
        feed.force_resync();
        feed.flush_resync(FrontendEpoch::INITIAL, FrontendRevision::new(1));
        assert!(feed.needs_resync());

        assert_eq!(feed.push(snapshot_update(2)), FeedPush::Backpressured);
        assert!(feed.pending_critical());
        assert!(feed.needs_resync());

        assert!(matches!(
            critical_rx.try_recv().unwrap().kind,
            FrontendUpdateKind::ResyncRequired { .. }
        ));
        assert_eq!(feed.flush_pending_critical(), Some(FeedPush::Sent));
        assert!(matches!(
            critical_rx.try_recv().unwrap().kind,
            FrontendUpdateKind::SnapshotReady(_)
        ));
        assert!(!feed.needs_resync());
    }
}
