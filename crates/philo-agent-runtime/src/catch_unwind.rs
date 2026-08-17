//! Catch panics from a polled future so a driver task can return
//! [`crate::DriverExit::Panicked`] instead of unwinding the JoinHandle.

use std::future::Future;
use std::mem::ManuallyDrop;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::task::{Context, Poll};

pub(crate) struct CatchUnwind<F> {
    inner: ManuallyDrop<AssertUnwindSafe<F>>,
    panicked: bool,
}

pub(crate) fn catch_unwind_async<F: Future>(future: F) -> CatchUnwind<F> {
    CatchUnwind {
        inner: ManuallyDrop::new(AssertUnwindSafe(future)),
        panicked: false,
    }
}

impl<F: Future> Future for CatchUnwind<F> {
    type Output = Result<F::Output, Box<dyn std::any::Any + Send>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: we never move the inner future; only poll it in place.
        let this = unsafe { self.get_unchecked_mut() };
        let inner = unsafe { Pin::new_unchecked(&mut this.inner.0) };
        match catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Err(payload) => {
                this.panicked = true;
                Poll::Ready(Err(payload))
            }
        }
    }
}

impl<F> Drop for CatchUnwind<F> {
    fn drop(&mut self) {
        if !self.panicked {
            // SAFETY: the future completed or was cancelled without panicking.
            unsafe { ManuallyDrop::drop(&mut self.inner) }
        }
        // A future that panicked during poll is leaked. Dropping it after a
        // mid-poll panic can re-enter Drop in an invalid state and hang the
        // worker (observed on Windows test runtimes).
    }
}
