//! Fair selection across agent events, terminal input and redraw ticks.

use std::collections::VecDeque;
use std::future::poll_fn;
use std::pin::Pin;

use crossterm::event::{Event as TermEvent, EventStream};
use futures_core::Stream;
use philo_agent_runtime::{AgentEvent, OperationHandle};

pub(crate) enum Step {
    Agent(Option<AgentEvent>),
    Term(std::io::Result<TermEvent>),
    TermClosed,
    Tick,
}

/// Rebuilding the handle future each poll is sound: event arrival wakes
/// the task and the rebuilt future resolves immediately.
pub(crate) async fn next_step(
    handles: &mut VecDeque<OperationHandle>,
    term_events: &mut EventStream,
    redraw_tick: &mut tokio::time::Interval,
) -> Step {
    tokio::select! {
        event = next_agent_event(handles) => Step::Agent(event),
        event = next_terminal_event(term_events) => match event {
            Some(result) => Step::Term(result),
            None => Step::TermClosed,
        },
        _ = redraw_tick.tick() => Step::Tick,
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
