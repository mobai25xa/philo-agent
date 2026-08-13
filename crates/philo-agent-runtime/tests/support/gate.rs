use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;

/// One-way latch used to suspend fake ports at deterministic points.
///
/// A gated future stays `Pending` until `release()`; the spin-polling test
/// executor re-polls, so no waker plumbing is needed.
#[derive(Clone, Debug, Default)]
pub struct Gate(Arc<AtomicBool>);

impl Gate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn release(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_released(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub async fn wait(&self) {
        std::future::poll_fn(|_| {
            if self.is_released() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
}
