//! Replaceable terminal input. Production wraps crossterm `EventStream`.

use std::future::poll_fn;
use std::io::ErrorKind;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

#[cfg(test)]
use std::collections::VecDeque;
#[cfg(test)]
use std::task::Waker;

use crossterm::event::{Event as TermEvent, EventStream, KeyEvent, MouseEvent};
use futures_core::Stream;

/// Normalized terminal input after the platform adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalInput {
    Key(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    Resize { width: u16, height: u16 },
}

/// Structured input faults. The driver decides retry / restart / fallback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalInputFault {
    Interrupted,
    WouldBlock,
    ZeroSizeResize,
    InvalidHandle,
    StreamTerminated,
    ErrorBudgetExceeded { message: String },
}

/// Driver-facing input source. Tests inject a fake implementation.
pub trait TerminalInputSource {
    /// Next input or fault. `None` means the source ended.
    fn poll_next(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<TerminalInput, TerminalInputFault>>>;

    /// Rebuild after an invalid handle. Bounded backoff belongs to the driver.
    fn rebuild(&mut self) -> Result<(), TerminalInputFault> {
        Ok(())
    }
}

impl dyn TerminalInputSource + '_ {
    /// Await the next item from a source.
    pub async fn next_async(
        source: &mut impl TerminalInputSource,
    ) -> Option<Result<TerminalInput, TerminalInputFault>> {
        poll_fn(|cx| source.poll_next(cx)).await
    }
}

/// Production adapter around crossterm `EventStream`.
pub struct CrosstermInputSource {
    stream: EventStream,
}

impl CrosstermInputSource {
    pub fn new() -> Self {
        Self {
            stream: EventStream::new(),
        }
    }
}

impl Default for CrosstermInputSource {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalInputSource for CrosstermInputSource {
    fn poll_next(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<TerminalInput, TerminalInputFault>>> {
        match Pin::new(&mut self.stream).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(Some(Err(TerminalInputFault::StreamTerminated))),
            Poll::Ready(Some(Ok(event))) => Poll::Ready(Some(classify_event(event))),
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(classify_io_error(&error)))),
        }
    }

    fn rebuild(&mut self) -> Result<(), TerminalInputFault> {
        self.stream = EventStream::new();
        Ok(())
    }
}

fn classify_event(event: TermEvent) -> Result<TerminalInput, TerminalInputFault> {
    match event {
        TermEvent::Key(key) => Ok(TerminalInput::Key(key)),
        TermEvent::Paste(text) => Ok(TerminalInput::Paste(text)),
        TermEvent::Mouse(mouse) => Ok(TerminalInput::Mouse(mouse)),
        TermEvent::Resize(width, height) if width == 0 || height == 0 => {
            Err(TerminalInputFault::ZeroSizeResize)
        }
        TermEvent::Resize(width, height) => Ok(TerminalInput::Resize { width, height }),
        _ => Err(TerminalInputFault::WouldBlock),
    }
}

fn classify_io_error(error: &std::io::Error) -> TerminalInputFault {
    match error.kind() {
        ErrorKind::Interrupted => TerminalInputFault::Interrupted,
        ErrorKind::WouldBlock => TerminalInputFault::WouldBlock,
        ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof | ErrorKind::NotConnected => {
            TerminalInputFault::StreamTerminated
        }
        // Handle-shaped OS errors and every other kind degrade the same
        // way: the input source is rebuilt or the run falls back.
        _ => TerminalInputFault::InvalidHandle,
    }
}

/// Consecutive-error budget and invalid-handle backoff for the driver.
#[derive(Debug)]
pub(crate) struct InputFaultTracker {
    consecutive: u32,
    rebuilds: u32,
    budget: u32,
}

impl InputFaultTracker {
    pub(crate) fn new(budget: u32) -> Self {
        Self {
            consecutive: 0,
            rebuilds: 0,
            budget,
        }
    }

    pub(crate) fn ok(&mut self) {
        self.consecutive = 0;
    }

    pub(crate) fn fail(&mut self) -> bool {
        self.consecutive = self.consecutive.saturating_add(1);
        self.consecutive > self.budget
    }

    pub(crate) fn rebuild_backoff(&mut self) -> Duration {
        self.rebuilds = self.rebuilds.saturating_add(1);
        let shift = (self.rebuilds.saturating_sub(1)).min(4);
        Duration::from_millis(10u64.saturating_mul(1 << shift))
    }

    pub(crate) fn rebuilds_exhausted(&self) -> bool {
        self.rebuilds >= 5
    }
}

/// Deterministic source for tests.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct FakeInputSource {
    items: VecDeque<Result<TerminalInput, TerminalInputFault>>,
    rebuilds: u32,
    waiting_for_rebuild: bool,
    waker: Option<Waker>,
    invalid_handle: Option<std::sync::Arc<tokio::sync::Notify>>,
}

#[cfg(test)]
impl FakeInputSource {
    pub fn new(items: impl IntoIterator<Item = Result<TerminalInput, TerminalInputFault>>) -> Self {
        Self {
            items: items.into_iter().collect(),
            rebuilds: 0,
            waiting_for_rebuild: false,
            waker: None,
            invalid_handle: None,
        }
    }

    pub fn rebuilds(&self) -> u32 {
        self.rebuilds
    }

    /// Notifies once when this source yields [`TerminalInputFault::InvalidHandle`].
    pub fn notify_on_invalid_handle(&mut self, notify: std::sync::Arc<tokio::sync::Notify>) {
        self.invalid_handle = Some(notify);
    }
}

#[cfg(test)]
impl TerminalInputSource for FakeInputSource {
    fn poll_next(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<TerminalInput, TerminalInputFault>>> {
        if self.waiting_for_rebuild {
            self.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let item = self.items.pop_front();
        if matches!(item, Some(Err(TerminalInputFault::InvalidHandle))) {
            self.waiting_for_rebuild = true;
            if let Some(notify) = self.invalid_handle.take() {
                notify.notify_one();
            }
        }
        Poll::Ready(item)
    }

    fn rebuild(&mut self) -> Result<(), TerminalInputFault> {
        self.rebuilds += 1;
        self.waiting_for_rebuild = false;
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_size_resize_is_a_fault_not_an_exit() {
        assert_eq!(
            classify_event(TermEvent::Resize(0, 24)),
            Err(TerminalInputFault::ZeroSizeResize)
        );
        assert_eq!(
            classify_event(TermEvent::Resize(80, 0)),
            Err(TerminalInputFault::ZeroSizeResize)
        );
    }

    #[test]
    fn interrupted_and_would_block_map_to_retry_faults() {
        assert_eq!(
            classify_io_error(&std::io::Error::from(ErrorKind::Interrupted)),
            TerminalInputFault::Interrupted
        );
        assert_eq!(
            classify_io_error(&std::io::Error::from(ErrorKind::WouldBlock)),
            TerminalInputFault::WouldBlock
        );
    }

    #[test]
    fn fake_source_yields_injected_items_then_ends() {
        let mut source = FakeInputSource::new([
            Err(TerminalInputFault::Interrupted),
            Ok(TerminalInput::Resize {
                width: 80,
                height: 24,
            }),
        ]);
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert_eq!(
            source.poll_next(&mut cx),
            Poll::Ready(Some(Err(TerminalInputFault::Interrupted)))
        );
        assert!(matches!(
            source.poll_next(&mut cx),
            Poll::Ready(Some(Ok(TerminalInput::Resize { .. })))
        ));
        assert_eq!(source.poll_next(&mut cx), Poll::Ready(None));
    }

    #[test]
    fn invalid_handle_holds_until_rebuild() {
        let mut source = FakeInputSource::new([Err(TerminalInputFault::InvalidHandle)]);
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert_eq!(
            source.poll_next(&mut cx),
            Poll::Ready(Some(Err(TerminalInputFault::InvalidHandle)))
        );
        assert_eq!(source.poll_next(&mut cx), Poll::Pending);
        assert_eq!(source.rebuilds(), 0);
        source.rebuild().expect("rebuild");
        assert_eq!(source.rebuilds(), 1);
        assert_eq!(source.poll_next(&mut cx), Poll::Ready(None));
    }
}
