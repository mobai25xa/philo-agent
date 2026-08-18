//! Bounded frontend feed. Overflow marks `needs_resync` and later emits
//! only [`crate::FrontendUpdateKind::ResyncRequired`].

use tokio::sync::mpsc;

use crate::frontend::update::{FrontendUpdate, FrontendUpdateKind};
use crate::ids::{FrontendEpoch, FrontendRevision};

/// Why the feed could not accept an update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedPush {
    /// Update is on the lane.
    Sent,
    /// Lane is in resync; the update was dropped.
    Dropped,
    /// Receiver is gone.
    Disconnected,
}

/// Error raised when a consumer must snapshot before applying more updates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResyncRequired {
    /// Service revision to request.
    pub latest_revision: FrontendRevision,
}

/// Bounded sender side of the frontend update lane.
pub(crate) struct FrontendFeed {
    tx: mpsc::Sender<FrontendUpdate>,
    needs_resync: bool,
    awaiting_snapshot: bool,
    disconnected: bool,
}

impl FrontendFeed {
    pub(crate) fn new(tx: mpsc::Sender<FrontendUpdate>) -> Self {
        Self {
            tx,
            needs_resync: false,
            awaiting_snapshot: false,
            disconnected: false,
        }
    }

    pub(crate) fn pending_resync(&self) -> bool {
        self.needs_resync && !self.disconnected && !self.awaiting_snapshot
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
    /// Command replies (`request_id` set) still go through during resync so a
    /// submit/cancel cannot hang without a terminal result.
    pub(crate) fn push(&mut self, update: FrontendUpdate) -> FeedPush {
        if self.disconnected {
            return FeedPush::Disconnected;
        }
        let is_command_reply = update.request_id.is_some();
        if self.awaiting_snapshot && !is_command_reply {
            return FeedPush::Dropped;
        }
        if self.needs_resync && !is_command_reply {
            self.try_resync(update.epoch, update.revision);
            return FeedPush::Dropped;
        }
        match self.tx.try_send(update) {
            Ok(()) => FeedPush::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => {
                if is_command_reply {
                    FeedPush::Dropped
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

    fn try_resync(&mut self, epoch: FrontendEpoch, revision: FrontendRevision) {
        let update = FrontendUpdate::new(
            epoch,
            revision,
            None,
            FrontendUpdateKind::ResyncRequired {
                latest_revision: revision,
            },
        );
        match self.tx.try_send(update) {
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

    /// SnapshotReady was handed to the feed (or is about to be). Resume deltas.
    pub(crate) fn on_snapshot_ready(&mut self) {
        self.needs_resync = false;
        self.awaiting_snapshot = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::update::FrontendUpdateKind;
    use crate::ids::FrontendEpoch;

    fn update(revision: u64) -> FrontendUpdate {
        FrontendUpdate::new(
            FrontendEpoch::INITIAL,
            FrontendRevision::new(revision),
            None,
            FrontendUpdateKind::CommandAccepted,
        )
    }

    #[test]
    fn overflow_is_bounded_and_requests_resync() {
        let (tx, mut rx) = mpsc::channel(2);
        let mut feed = FrontendFeed::new(tx);
        assert_eq!(feed.push(update(1)), FeedPush::Sent);
        assert_eq!(feed.push(update(2)), FeedPush::Sent);
        assert_eq!(feed.push(update(3)), FeedPush::Dropped);
        assert!(feed.needs_resync());

        let first = rx.try_recv().unwrap();
        assert!(matches!(first.kind, FrontendUpdateKind::CommandAccepted));
        feed.flush_resync(FrontendEpoch::INITIAL, FrontendRevision::new(3));
        // Channel still holds the second CommandAccepted; resync waits.
        assert!(feed.needs_resync());
        let _ = rx.try_recv().unwrap();
        feed.flush_resync(FrontendEpoch::INITIAL, FrontendRevision::new(3));
        let resync = rx.try_recv().unwrap();
        assert!(matches!(
            resync.kind,
            FrontendUpdateKind::ResyncRequired { .. }
        ));
        assert_eq!(feed.push(update(4)), FeedPush::Dropped);
        feed.on_snapshot_ready();
        assert_eq!(feed.push(update(5)), FeedPush::Sent);
    }

    #[test]
    fn closed_receiver_is_disconnected_not_blocking() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let mut feed = FrontendFeed::new(tx);
        assert_eq!(feed.push(update(1)), FeedPush::Disconnected);
        assert!(feed.is_disconnected());
    }
}
