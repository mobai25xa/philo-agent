//! Bounded runtime event subscription. Does not expose the mpsc type.

use crate::{RuntimeEvent, TryRecvError};
use tokio::sync::mpsc;

/// Single bounded event outlet from one runtime epoch.
pub struct RuntimeSubscription {
    pub(crate) events: mpsc::Receiver<RuntimeEvent>,
}

impl RuntimeSubscription {
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
