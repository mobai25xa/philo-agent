//! Bounded runtime event receiver. Does not expose the mpsc type.
//!
//! The receiver can be taken once from [`crate::RuntimeParts`]. There is no
//! public `subscribe()` that copies the reliable outlet.

use crate::{RuntimeEvent, TryRecvError};
use tokio::sync::mpsc;

/// Single bounded event outlet from one runtime epoch.
pub struct RuntimeEventReceiver {
    pub(crate) events: mpsc::Receiver<RuntimeEvent>,
}

impl RuntimeEventReceiver {
    pub async fn recv(&mut self) -> Option<RuntimeEvent> {
        self.events.recv().await
    }

    pub fn try_recv(&mut self) -> Result<RuntimeEvent, TryRecvError> {
        match self.events.try_recv() {
            Ok(event) => Ok(event),
            Err(mpsc::error::TryRecvError::Empty) => Err(TryRecvError::Empty),
            Err(mpsc::error::TryRecvError::Disconnected) => Err(TryRecvError::Closed),
        }
    }
}
