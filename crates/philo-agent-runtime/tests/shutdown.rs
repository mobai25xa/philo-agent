//! Shutdown signal, deadline, and shared epoch finalizer.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use philo_agent_runtime::{
    AdmissionError, ChannelBounds, GenerationConfig, RuntimeConfig, RuntimeEvent, SequentialIdSource,
    ShutdownError, ShutdownMode, ShutdownState,
};
use philo_session::MemorySessionStore;
use support::fake_model::{FakeModel, ModelScript};
use support::gate::Gate;
use support::runtime::{
    EventProbe, empty_tools, generation, start_with_bounds, submit_prompt, wait_until_busy,
    wait_until_idle, wait_until_queued, wait_until_shutdown_leaves_running,
};

fn config() -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "sys".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds: 1,
        max_parallel_tool_calls: 1,
        operation_timeout: None,
        tool_cancel_grace: Duration::from_millis(300),
        compaction: Default::default(),
    }
}

fn control_cap_bounds(control_cap: usize) -> ChannelBounds {
    ChannelBounds {
        command_cap: 32,
        control_cap,
        event_cap: 32,
        queue_max: 32,
        driver_event_budget: 32,
        reliable_staging_cap: 64,
    }
}

fn unread_event_bounds() -> ChannelBounds {
    ChannelBounds {
        command_cap: 4,
        control_cap: 8,
        event_cap: 1,
        queue_max: 4,
        driver_event_budget: 8,
        reliable_staging_cap: 4,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_observed_when_control_mailbox_full() {
    let model = Arc::new(FakeModel::succeeds(&["hello"]));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, mut sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        control_cap_bounds(1),
    )
    .await;
    let accepted = submit_prompt(
        &handle,
        generation(model, empty_tools(), config()),
        "session",
        "hi",
    )
    .await;
    wait_until_idle(&handle).await;

    while handle.try_send_control_probe() {}
    assert!(
        !handle.try_send_control_probe(),
        "control mailbox should be full"
    );

    let report = tokio::time::timeout(
        Duration::from_secs(2),
        handle.shutdown(ShutdownMode::Forced, Instant::now() + Duration::from_secs(1)),
    )
    .await
    .expect("shutdown must not hang behind a full control mailbox")
    .expect("full control mailbox must not forge RuntimeGone");
    assert_eq!(report.final_state, ShutdownState::Stopped);
    assert!(
        report.settlements.is_empty(),
        "idle shutdown should not invent forced settlements"
    );

    let ended = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match sub.recv().await {
                Some(RuntimeEvent::EpochEnded { .. }) => return true,
                Some(_) => {}
                None => return false,
            }
        }
    })
    .await
    .expect("event drain");
    assert!(ended, "supervisor must publish EpochEnded");
    let _ = accepted;
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_respects_deadline_when_event_outlet_unread() {
    let gate = Gate::new();
    let model = Arc::new(FakeModel::new([ModelScript::text_suspending(
        &[],
        &gate,
        &["done"],
    )]));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        unread_event_bounds(),
    )
    .await;
    let probe = EventProbe::start_paused(sub);
    let accepted = submit_prompt(
        &handle,
        generation(model, empty_tools(), config()),
        "session",
        "hi",
    )
    .await;
    wait_until_busy(&handle, &accepted.operation_id).await;

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        handle.shutdown(
            ShutdownMode::Forced,
            Instant::now() + Duration::from_millis(200),
        ),
    )
    .await
    .expect("unread outlet must not hang shutdown");
    match result {
        Ok(report) => assert_eq!(report.final_state, ShutdownState::Stopped),
        Err(ShutdownError::DeadlineExceeded { pending }) => {
            assert!(
                pending.iter().any(|name| name.contains("runtime")),
                "deadline must name the pending component, got {pending:?}"
            );
        }
        Err(other) => panic!("unexpected shutdown error: {other:?}"),
    }
    gate.release();
    drop(probe);
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_event_receiver_still_finalizes() {
    let gate = Gate::new();
    let model = Arc::new(FakeModel::new([ModelScript::text_suspending(
        &[],
        &gate,
        &["done"],
    )]));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        unread_event_bounds(),
    )
    .await;
    let accepted = submit_prompt(
        &handle,
        generation(model, empty_tools(), config()),
        "session",
        "hi",
    )
    .await;
    wait_until_busy(&handle, &accepted.operation_id).await;
    drop(sub);
    tokio::time::timeout(Duration::from_secs(2), wait_until_shutdown_leaves_running(&handle))
        .await
        .expect("drop receiver must finalize");
    let err = handle
        .submit(philo_agent_runtime::OperationSpec {
            session_id: philo_agent_runtime::SessionId::new("session"),
            user_message: philo_agent_runtime::UserMessage::new("again"),
            generation: generation(
                Arc::new(FakeModel::succeeds(&["x"])),
                empty_tools(),
                config(),
            ),
            service_request_id: None,
        })
        .await
        .expect_err("admission must stop");
    assert!(matches!(
        err,
        AdmissionError::ShuttingDown | AdmissionError::RuntimeStopped | AdmissionError::Backpressured
    ));
    gate.release();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coordinator_panic_one_forced_settlement_per_accepted() {
    let gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &gate, &["active"]),
        ModelScript::text_suspending(&[], &gate, &["queued"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, mut sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        control_cap_bounds(16),
    )
    .await;
    let generation = generation(model, empty_tools(), config());
    let first = submit_prompt(&handle, generation.clone(), "session-a", "one").await;
    wait_until_busy(&handle, &first.operation_id).await;
    let second = submit_prompt(&handle, generation, "session-b", "two").await;
    wait_until_queued(&handle, &second.operation_id).await;
    handle.inject_coordinator_panic().await;

    let report = tokio::time::timeout(
        Duration::from_secs(3),
        handle.shutdown(ShutdownMode::Forced, Instant::now() + Duration::from_secs(2)),
    )
    .await
    .expect("panic finalizer must not hang")
    .expect("supervisor must complete after panic");
    assert_eq!(report.final_state, ShutdownState::Stopped);
    let forced: Vec<_> = report
        .settlements
        .iter()
        .map(|item| item.operation_id.clone())
        .collect();
    assert_eq!(forced.len(), 2);
    assert!(forced.contains(&first.operation_id));
    assert!(forced.contains(&second.operation_id));
    for settlement in &report.settlements {
        assert!(!settlement.session_id.as_str().is_empty());
    }

    let mut settled = Vec::new();
    let mut forced_count = None;
    let drain = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = sub.recv().await {
            match event {
                RuntimeEvent::OperationSettled { operation_id, .. } => {
                    settled.push(operation_id);
                }
                RuntimeEvent::EpochEnded {
                    forced_count: count,
                    ..
                } => {
                    forced_count = Some(count);
                    break;
                }
                _ => {}
            }
        }
    });
    drain.await.expect("drain after panic");
    assert_eq!(forced_count, Some(2));
    assert_eq!(handle.snapshot().await.shutdown, ShutdownState::Stopped);
    gate.release();
}

#[tokio::test(flavor = "current_thread")]
async fn repeated_shutdown_is_idempotent() {
    let model = Arc::new(FakeModel::succeeds(&["hello"]));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, _sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        control_cap_bounds(8),
    )
    .await;
    submit_prompt(
        &handle,
        generation(model, empty_tools(), config()),
        "session",
        "hi",
    )
    .await;
    wait_until_idle(&handle).await;
    let first = handle
        .shutdown(
            ShutdownMode::Drain,
            Instant::now() + Duration::from_secs(2),
        )
        .await
        .expect("first shutdown");
    let second = handle
        .shutdown(
            ShutdownMode::Forced,
            Instant::now() + Duration::from_secs(2),
        )
        .await
        .expect("second shutdown");
    assert_eq!(first.epoch, second.epoch);
    assert_eq!(first.final_state, ShutdownState::Stopped);
    assert_eq!(second.final_state, ShutdownState::Stopped);
}
