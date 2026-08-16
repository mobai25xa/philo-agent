//! Fair selection across agent events, terminal input, task completion, and
//! render/animation deadlines.

use std::collections::VecDeque;
use std::future::Future;
use std::future::poll_fn;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use crossterm::event::{Event as TermEvent, EventStream};
use futures_core::Stream;
use philo_agent_runtime::{
    AgentEvent, CompactionError, CompactionReport, OperationHandle, RuntimeFuture,
};

use crate::api::types::ConfigReloadNotice;

use super::tasks::{PendingTasks, TaskCompletion};

pub(crate) type CompactionFuture =
    RuntimeFuture<'static, Result<CompactionReport, CompactionError>>;

pub(crate) enum Step {
    Agent(Option<AgentEvent>),
    Task(TaskCompletion),
    Compaction(Result<CompactionReport, CompactionError>),
    Term(std::io::Result<TermEvent>),
    TermClosed,
    FrameDeadline,
    AnimationDeadline,
    ConfirmationPoll,
    ConfigNotice(ConfigReloadNotice),
}

pub(crate) enum AgentItem {
    Event(AgentEvent),
    Settled,
}

/// A ready AgentEvent source is drained only to this bound before the loop
/// returns to fair selection with terminal/control sources.
pub(crate) const MAX_AGENT_EVENTS_PER_ROUND: usize = 64;

/// Rebuilding the handle future each poll is sound: event arrival wakes
/// the task and the rebuilt future resolves immediately.
pub(crate) async fn next_step(
    handles: &mut VecDeque<OperationHandle>,
    tasks: &mut PendingTasks,
    compaction: &mut Option<CompactionFuture>,
    term_events: &mut EventStream,
    frame_deadline: Option<tokio::time::Instant>,
    animation_deadline: Option<tokio::time::Instant>,
    confirmation_poll: Option<tokio::time::Instant>,
    config_notices: &mut Option<tokio::sync::mpsc::UnboundedReceiver<ConfigReloadNotice>>,
) -> Step {
    tokio::select! {
        event = next_agent_event(handles) => Step::Agent(event),
        completion = tasks.next_completion() => Step::Task(completion),
        result = next_compaction(compaction) => Step::Compaction(result),
        event = next_terminal_event(term_events) => match event {
            Some(result) => Step::Term(result),
            None => Step::TermClosed,
        },
        _ = wait_for(frame_deadline) => Step::FrameDeadline,
        _ = wait_for(animation_deadline) => Step::AnimationDeadline,
        _ = wait_for(confirmation_poll) => Step::ConfirmationPoll,
        notice = next_config_notice(config_notices) => Step::ConfigNotice(notice),
    }
}

/// Includes the item selected by `next_step`, then polls already-ready
/// events without waiting. Closed handles are popped in sequence so FIFO
/// operations can advance within the same bounded round.
pub(crate) fn drain_ready_agent_items(
    handles: &mut VecDeque<OperationHandle>,
    first: Option<AgentEvent>,
) -> Vec<AgentItem> {
    let first = classify_agent_item(handles, first);
    take_bounded(first, MAX_AGENT_EVENTS_PER_ROUND, || {
        let event = poll_agent_event(handles)?;
        Some(classify_agent_item(handles, event))
    })
}

fn classify_agent_item(
    handles: &mut VecDeque<OperationHandle>,
    event: Option<AgentEvent>,
) -> AgentItem {
    match event {
        Some(event) => AgentItem::Event(event),
        None => {
            handles.pop_front();
            AgentItem::Settled
        }
    }
}

fn poll_agent_event(handles: &mut VecDeque<OperationHandle>) -> Option<Option<AgentEvent>> {
    let handle = handles.front_mut()?;
    let mut future = std::pin::pin!(handle.next_event());
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(event) => Some(event),
        Poll::Pending => None,
    }
}

fn take_bounded<T>(first: T, limit: usize, mut next_ready: impl FnMut() -> Option<T>) -> Vec<T> {
    debug_assert!(limit > 0);
    let mut items = Vec::with_capacity(limit);
    items.push(first);
    while items.len() < limit {
        let Some(item) = next_ready() else {
            break;
        };
        items.push(item);
    }
    items
}

async fn wait_for(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

async fn next_compaction(
    compaction: &mut Option<CompactionFuture>,
) -> Result<CompactionReport, CompactionError> {
    match compaction {
        Some(future) => future.as_mut().await,
        None => std::future::pending::<Result<CompactionReport, CompactionError>>().await,
    }
}

async fn next_agent_event(handles: &mut VecDeque<OperationHandle>) -> Option<AgentEvent> {
    match handles.front_mut() {
        Some(handle) => handle.next_event().await,
        None => std::future::pending::<Option<AgentEvent>>().await,
    }
}

async fn next_terminal_event(term_events: &mut EventStream) -> Option<std::io::Result<TermEvent>> {
    poll_fn(|cx| Pin::new(&mut *term_events).poll_next(cx)).await
}

async fn next_config_notice(
    notices: &mut Option<tokio::sync::mpsc::UnboundedReceiver<ConfigReloadNotice>>,
) -> ConfigReloadNotice {
    match notices.as_mut() {
        Some(rx) => match rx.recv().await {
            Some(notice) => notice,
            None => {
                *notices = None;
                std::future::pending().await
            }
        },
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_ready_drain_preserves_order_and_leaves_work_for_fair_selection() {
        let mut ready: VecDeque<usize> = (1..=MAX_AGENT_EVENTS_PER_ROUND + 5).collect();
        let drained = take_bounded(0, MAX_AGENT_EVENTS_PER_ROUND, || ready.pop_front());

        assert_eq!(drained.len(), MAX_AGENT_EVENTS_PER_ROUND);
        assert_eq!(drained, (0..MAX_AGENT_EVENTS_PER_ROUND).collect::<Vec<_>>());
        assert_eq!(ready.front(), Some(&MAX_AGENT_EVENTS_PER_ROUND));
    }
}
