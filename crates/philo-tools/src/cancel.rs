use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use crate::{RichToolResult, ToolProgressSink};

/// Cooperative stop signal injected into each `invoke`.
///
/// Requesting never fails. Handlers may ignore the token; Runtime then
/// waits out a grace window and drops the invoke future.
#[derive(Clone)]
pub struct ToolCancel {
    inner: Arc<Inner>,
}

struct Inner {
    requested: AtomicBool,
    wakers: Mutex<Vec<Waker>>,
}

impl ToolCancel {
    /// A fresh token that becomes cancelled only after [`ToolCancel::request`].
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                requested: AtomicBool::new(false),
                wakers: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Same as [`ToolCancel::new`]: a token nobody has requested yet.
    pub fn none() -> Self {
        Self::new()
    }

    /// Marks this token cancelled and wakes every waiter.
    pub fn request(&self) {
        self.inner.requested.store(true, Ordering::SeqCst);
        let wakers = {
            let mut slot = self
                .inner
                .wakers
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            std::mem::take(&mut *slot)
        };
        for waker in wakers {
            waker.wake();
        }
    }

    /// Returns whether [`ToolCancel::request`] has been called.
    pub fn is_requested(&self) -> bool {
        self.inner.requested.load(Ordering::SeqCst)
    }

    /// Resolves when [`ToolCancel::request`] has been called.
    pub fn cancelled(&self) -> ToolCancelled {
        ToolCancelled {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Default for ToolCancel {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ToolCancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCancel")
            .field("requested", &self.is_requested())
            .finish()
    }
}

/// Future returned by [`ToolCancel::cancelled`].
pub struct ToolCancelled {
    inner: Arc<Inner>,
}

impl Future for ToolCancelled {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.inner.requested.load(Ordering::SeqCst) {
            return Poll::Ready(());
        }
        {
            let mut wakers = self
                .inner
                .wakers
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            wakers.push(cx.waker().clone());
        }
        if self.inner.requested.load(Ordering::SeqCst) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

/// Per-invoke context: display sink plus the operation cancel token.
#[derive(Clone)]
pub struct ToolInvokeCx {
    progress: ToolProgressSink,
    cancel: ToolCancel,
}

impl ToolInvokeCx {
    /// Builds a context from an explicit sink and token.
    pub fn new(progress: ToolProgressSink, cancel: ToolCancel) -> Self {
        Self { progress, cancel }
    }

    /// No-op sink and a never-requested token.
    pub fn ignore() -> Self {
        Self::new(ToolProgressSink::noop(), ToolCancel::none())
    }

    /// Live display sink only; cancel is never requested unless the caller
    /// later shares the same [`ToolCancel`].
    pub fn progress_only(progress: ToolProgressSink) -> Self {
        Self::new(progress, ToolCancel::none())
    }

    /// Display-channel sink for this invoke.
    pub fn progress(&self) -> &ToolProgressSink {
        &self.progress
    }

    /// Cancel token for this invoke.
    pub fn cancel(&self) -> &ToolCancel {
        &self.cancel
    }
}

impl Default for ToolInvokeCx {
    fn default() -> Self {
        Self::ignore()
    }
}

impl std::fmt::Debug for ToolInvokeCx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolInvokeCx")
            .field("cancel", &self.cancel)
            .finish()
    }
}

/// How one `invoke` ended. `Stopped` is Port-only and never a Session fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolInvokeEnd {
    /// Complete, trustworthy model-channel result.
    Done(RichToolResult),
    /// The call started but has no trustworthy complete result.
    Stopped,
}

impl ToolInvokeEnd {
    /// Wraps a complete result.
    pub fn done(result: RichToolResult) -> Self {
        Self::Done(result)
    }

    /// The complete result when this end is [`ToolInvokeEnd::Done`].
    pub fn as_done(&self) -> Option<&RichToolResult> {
        match self {
            Self::Done(result) => Some(result),
            Self::Stopped => None,
        }
    }

    /// Unwraps a complete result.
    pub fn into_done(self) -> Option<RichToolResult> {
        match self {
            Self::Done(result) => Some(result),
            Self::Stopped => None,
        }
    }
}
