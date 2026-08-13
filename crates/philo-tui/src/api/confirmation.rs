//! Public confirmation channel: the UI conduit for external approval
//! decorators. The TUI only carries the question in and the answer out;
//! approval semantics (policy, always/session memory, defaults) belong to
//! the external decorator. Nothing here persists or enters the session.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// One approval question shown in the overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmationRequest {
    pub title: String,
    pub body: String,
}

/// The user's answer. Closing the overlay (`Esc`) denies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmationResponse {
    Allow,
    Deny,
}

/// Identifies one pending request within the channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConfirmationId(u64);

impl ConfirmationId {
    /// Builds an id without going through the queue, for overlay tests.
    #[cfg(test)]
    pub(crate) fn for_test(raw: u64) -> Self {
        Self(raw)
    }
}

struct PendingRequest {
    id: ConfirmationId,
    request: ConfirmationRequest,
    slot: Arc<Mutex<ResponseSlot>>,
}

#[derive(Default)]
struct ResponseSlot {
    response: Option<ConfirmationResponse>,
    waker: Option<Waker>,
}

#[derive(Default)]
struct ChannelInner {
    next_id: u64,
    queue: VecDeque<PendingRequest>,
}

/// FIFO question/answer channel between an external approval decorator and
/// the TUI overlay. Cloning shares the same channel.
#[derive(Clone, Default)]
pub struct ConfirmationChannel {
    inner: Arc<Mutex<ChannelInner>>,
}

impl ConfirmationChannel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decorator side: submits a question and resolves with the answer.
    /// The future is runtime-agnostic and safe to await inside a tool-port
    /// decorator.
    pub fn request(
        &self,
        request: ConfirmationRequest,
    ) -> impl Future<Output = ConfirmationResponse> + Send + use<> {
        let slot = Arc::new(Mutex::new(ResponseSlot::default()));
        {
            let mut inner = self.inner.lock().expect("confirmation mutex");
            inner.next_id += 1;
            let id = ConfirmationId(inner.next_id);
            inner.queue.push_back(PendingRequest {
                id,
                request,
                slot: slot.clone(),
            });
        }
        ResponseFuture { slot }
    }

    /// TUI side: the request currently at the front of the queue (one is
    /// presented at a time, FIFO).
    pub(crate) fn front(&self) -> Option<(ConfirmationId, ConfirmationRequest)> {
        let inner = self.inner.lock().expect("confirmation mutex");
        inner
            .queue
            .front()
            .map(|pending| (pending.id, pending.request.clone()))
    }

    /// TUI side: answers a pending request. Unknown ids are ignored
    /// (already answered or auto-denied).
    pub(crate) fn respond(&self, id: ConfirmationId, response: ConfirmationResponse) {
        let mut inner = self.inner.lock().expect("confirmation mutex");
        if let Some(position) = inner.queue.iter().position(|pending| pending.id == id) {
            let pending = inner.queue.remove(position).expect("position exists");
            resolve(&pending.slot, response);
        }
    }

    /// Auto-denies every pending request. Called when the operation
    /// settles or is cancelled: no question may outlive its operation.
    pub(crate) fn deny_all(&self) {
        let drained: Vec<PendingRequest> = {
            let mut inner = self.inner.lock().expect("confirmation mutex");
            inner.queue.drain(..).collect()
        };
        for pending in drained {
            resolve(&pending.slot, ConfirmationResponse::Deny);
        }
    }

    /// True when no request is waiting.
    #[cfg(test)]
    pub(crate) fn is_idle(&self) -> bool {
        self.inner
            .lock()
            .expect("confirmation mutex")
            .queue
            .is_empty()
    }
}

fn resolve(slot: &Arc<Mutex<ResponseSlot>>, response: ConfirmationResponse) {
    let waker = {
        let mut slot = slot.lock().expect("confirmation slot mutex");
        slot.response = Some(response);
        slot.waker.take()
    };
    if let Some(waker) = waker {
        waker.wake();
    }
}

struct ResponseFuture {
    slot: Arc<Mutex<ResponseSlot>>,
}

impl Future for ResponseFuture {
    type Output = ConfirmationResponse;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut slot = self.slot.lock().expect("confirmation slot mutex");
        if let Some(response) = slot.response {
            return Poll::Ready(response);
        }
        slot.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::pin;

    fn poll_once<F: Future>(future: &mut Pin<&mut F>) -> Poll<F::Output> {
        let mut context = Context::from_waker(Waker::noop());
        future.as_mut().poll(&mut context)
    }

    fn request(title: &str) -> ConfirmationRequest {
        ConfirmationRequest {
            title: title.to_owned(),
            body: format!("{title} body"),
        }
    }

    #[test]
    fn requests_resolve_fifo_with_the_given_answer() {
        let channel = ConfirmationChannel::new();
        let first = channel.request(request("first"));
        let second = channel.request(request("second"));
        let mut first = pin!(first);
        let mut second = pin!(second);
        assert!(poll_once(&mut first).is_pending());

        let (front_id, front) = channel.front().expect("first is queued");
        assert_eq!(front.title, "first");
        channel.respond(front_id, ConfirmationResponse::Allow);
        assert_eq!(
            poll_once(&mut first),
            Poll::Ready(ConfirmationResponse::Allow)
        );

        let (second_id, front) = channel.front().expect("second surfaces next");
        assert_eq!(front.title, "second");
        channel.respond(second_id, ConfirmationResponse::Deny);
        assert_eq!(
            poll_once(&mut second),
            Poll::Ready(ConfirmationResponse::Deny)
        );
        assert!(channel.is_idle());
    }

    #[test]
    fn deny_all_resolves_every_pending_request() {
        let channel = ConfirmationChannel::new();
        let first = channel.request(request("first"));
        let second = channel.request(request("second"));
        let mut first = pin!(first);
        let mut second = pin!(second);

        channel.deny_all();
        assert_eq!(
            poll_once(&mut first),
            Poll::Ready(ConfirmationResponse::Deny)
        );
        assert_eq!(
            poll_once(&mut second),
            Poll::Ready(ConfirmationResponse::Deny)
        );
        assert!(channel.is_idle());
    }

    #[test]
    fn responding_to_an_unknown_id_is_a_no_op() {
        let channel = ConfirmationChannel::new();
        let pending = channel.request(request("only"));
        let mut pending = pin!(pending);
        let (id, _) = channel.front().expect("queued");
        channel.respond(id, ConfirmationResponse::Allow);
        // Same id again: gone from the queue, nothing happens.
        channel.respond(id, ConfirmationResponse::Deny);
        assert_eq!(
            poll_once(&mut pending),
            Poll::Ready(ConfirmationResponse::Allow)
        );
    }
}
