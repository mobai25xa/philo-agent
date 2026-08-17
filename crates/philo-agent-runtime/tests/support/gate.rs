use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Poll, Waker};

/// One-way latch used to suspend fake ports at deterministic points.
#[derive(Clone, Debug, Default)]
pub struct Gate {
    released: Arc<AtomicBool>,
    wakers: Arc<Mutex<Vec<Waker>>>,
}

impl Gate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        let wakers = self
            .wakers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect::<Vec<_>>();
        for waker in wakers {
            waker.wake();
        }
    }

    pub fn is_released(&self) -> bool {
        self.released.load(Ordering::SeqCst)
    }

    pub async fn wait(&self) {
        std::future::poll_fn(|cx| {
            if self.is_released() {
                return Poll::Ready(());
            }
            self.wakers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(cx.waker().clone());
            if self.is_released() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
}
