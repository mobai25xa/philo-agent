//! Fair selection across frontend updates, terminal input, workers, and
//! render/animation deadlines.

use std::future::poll_fn;
use std::time::{Duration, Instant as StdInstant};

use philo_agent_service::{FrontendClient, RecvOutcome};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::platform::input::{TerminalInput, TerminalInputFault, TerminalInputSource};

use super::tasks::{PendingTasks, TaskCompletion};

/// A ready frontend source is drained only to this bound before the loop
/// returns to fair selection with terminal/control sources.
pub(crate) const MAX_UPDATES_PER_ROUND: usize = 64;

pub(crate) enum Step {
    Update(philo_agent_service::FrontendUpdate),
    UpdatesDisconnected,
    Task(TaskCompletion),
    Input(Result<TerminalInput, TerminalInputFault>),
    InputClosed,
    InputRebuildDue,
    FrameDeadline,
    AnimationDeadline,
    /// Supervisor Ctrl+C pulse. Never process-exit; the loop decides cancel/force.
    Interrupt,
}

pub(crate) async fn next_step(
    client: &FrontendClient,
    tasks: &mut PendingTasks,
    input: &mut impl TerminalInputSource,
    frame_deadline: Option<Instant>,
    animation_deadline: Option<Instant>,
    rebuild_deadline: Option<Instant>,
    interrupt: Option<&mut watch::Receiver<u64>>,
) -> Step {
    tokio::select! {
        outcome = client.recv_until_async(StdInstant::now() + Duration::from_secs(60 * 60)) => {
            match outcome {
                RecvOutcome::Update(update) => Step::Update(update),
                RecvOutcome::Disconnected => Step::UpdatesDisconnected,
                RecvOutcome::Timeout => Step::FrameDeadline,
            }
        }
        completion = tasks.next_completion() => Step::Task(completion),
        item = poll_input(input, rebuild_deadline.is_some()) => match item {
            Some(result) => Step::Input(result),
            None => Step::InputClosed,
        },
        _ = wait_for(rebuild_deadline) => Step::InputRebuildDue,
        _ = wait_for(frame_deadline) => Step::FrameDeadline,
        _ = wait_for(animation_deadline) => Step::AnimationDeadline,
        _ = wait_interrupt(interrupt) => Step::Interrupt,
    }
}

async fn poll_input(
    input: &mut impl TerminalInputSource,
    rebuilding: bool,
) -> Option<Result<TerminalInput, TerminalInputFault>> {
    if rebuilding {
        std::future::pending().await
    } else {
        poll_fn(|cx| input.poll_next(cx)).await
    }
}

/// After the first update, drain already-ready updates without waiting.
pub(crate) fn drain_ready_updates(
    client: &FrontendClient,
    first: philo_agent_service::FrontendUpdate,
) -> Vec<philo_agent_service::FrontendUpdate> {
    let mut updates = vec![first];
    while updates.len() < MAX_UPDATES_PER_ROUND {
        match client.try_recv() {
            RecvOutcome::Update(update) => updates.push(update),
            RecvOutcome::Timeout | RecvOutcome::Disconnected => break,
        }
    }
    updates
}

async fn wait_for(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

async fn wait_interrupt(rx: Option<&mut watch::Receiver<u64>>) {
    match rx {
        Some(rx) => {
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending::<()>().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::tasks::PendingTasks;
    use crate::platform::input::FakeInputSource;

    #[test]
    fn update_budget_is_the_frontend_cap() {
        assert_eq!(MAX_UPDATES_PER_ROUND, 64);
    }

    #[tokio::test]
    async fn rebuild_wait_prefers_frontend_update_over_closed_input() {
        let (service, client, _runtime) = philo_agent_service::testing::start_test_service();
        let _dispatch = client.try_command(philo_agent_service::FrontendCommand::ReadStatus);
        let mut input = FakeInputSource::new([]);
        let mut tasks = PendingTasks::new();
        let rebuild = Instant::now() + Duration::from_secs(60);
        let step = next_step(
            &client,
            &mut tasks,
            &mut input,
            None,
            None,
            Some(rebuild),
            None,
        )
        .await;
        assert!(
            matches!(step, Step::Update(_)),
            "rebuild wait must not poll a closed input source"
        );
        drop(service);
    }
}
