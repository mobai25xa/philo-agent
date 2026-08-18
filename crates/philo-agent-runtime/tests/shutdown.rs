//! Shutdown signal, deadline, and shared epoch finalizer.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use philo_agent_runtime::{
    AdmissionError, ChannelBounds, GenerationConfig, OperationPhase, OperationStatus,
    RuntimeConfig, RuntimeEvent, SequentialIdSource, SettlementRevision, ShutdownError,
    ShutdownMode, ShutdownState,
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

/// Paused outlet with room for one completed operation's reliable prefix
/// plus settlement. `can_poll_driver` / `can_reap_children` both require
/// two free staging slots, so cap=4 plus `event_cap=1` stalls before
/// `last_settled` is written.
fn paused_outlet_bounds() -> ChannelBounds {
    ChannelBounds {
        command_cap: 4,
        control_cap: 8,
        event_cap: 1,
        queue_max: 4,
        driver_event_budget: 8,
        reliable_staging_cap: 8,
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
        handle.shutdown(
            ShutdownMode::Forced,
            Instant::now() + Duration::from_secs(1),
        ),
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
    tokio::time::timeout(
        Duration::from_secs(2),
        wait_until_shutdown_leaves_running(&handle),
    )
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
        AdmissionError::ShuttingDown
            | AdmissionError::RuntimeStopped
            | AdmissionError::Backpressured
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
        handle.shutdown(
            ShutdownMode::Forced,
            Instant::now() + Duration::from_secs(2),
        ),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_outlet_settled_then_coordinator_panic_still_publishes_one_settlement() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let model = Arc::new(FakeModel::succeeds(&["hello"]));
        let sessions = Arc::new(MemorySessionStore::new());
        let (handle, sub) = start_with_bounds(
            sessions,
            Arc::new(SequentialIdSource::new()),
            paused_outlet_bounds(),
        )
        .await;
        let probe = EventProbe::start_paused(sub);
        let accepted = submit_prompt(
            &handle,
            generation(model, empty_tools(), config()),
            "session-a",
            "hi",
        )
        .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = handle.snapshot().await;
            if snapshot
                .last_settled
                .iter()
                .any(|settled| settled.operation_id == accepted.operation_id)
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "settlement must enter staging while the outlet is paused"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        handle.inject_coordinator_panic().await;
        probe.resume();
        let events = probe
            .wait_for(
                |events| {
                    events
                        .iter()
                        .any(|event| matches!(event, RuntimeEvent::EpochEnded { .. }))
                },
                Duration::from_secs(5),
            )
            .await;
        let settled: Vec<_> = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    RuntimeEvent::OperationSettled { operation_id, .. }
                        if *operation_id == accepted.operation_id
                )
            })
            .collect();
        assert_eq!(
            settled.len(),
            1,
            "staged settlement must survive coordinator panic: {events:?}"
        );
        match settled[0] {
            RuntimeEvent::OperationSettled { session_id, .. } => {
                assert_eq!(session_id.as_str(), "session-a");
            }
            other => panic!("{other:?}"),
        }
        let forced_count = events.iter().find_map(|event| match event {
            RuntimeEvent::EpochEnded { forced_count, .. } => Some(*forced_count),
            _ => None,
        });
        assert_eq!(
            forced_count,
            Some(0),
            "already staged settlement is not a leftover forced fact"
        );
    })
    .await
    .expect("paused settle then panic timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver_join_keeps_reserve_and_stages_settlement() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let model = Arc::new(FakeModel::panics("driver panic after start"));
        let sessions = Arc::new(MemorySessionStore::new());
        let (handle, sub) = start_with_bounds(
            sessions,
            Arc::new(SequentialIdSource::new()),
            paused_outlet_bounds(),
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
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = handle.snapshot().await;
            if snapshot
                .last_settled
                .iter()
                .any(|settled| settled.operation_id == accepted.operation_id)
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "driver-join settlement must enter staging"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let snapshot = handle.snapshot().await;
        assert!(
            snapshot
                .last_settled
                .iter()
                .any(|settled| settled.operation_id == accepted.operation_id),
            "settle_operation must record only after staging succeeds"
        );

        probe.resume();
        let events = probe
            .wait_for(
                |events| {
                    events.iter().any(|event| {
                        matches!(
                            event,
                            RuntimeEvent::OperationSettled { operation_id, .. }
                                if *operation_id == accepted.operation_id
                        )
                    })
                },
                Duration::from_secs(5),
            )
            .await;
        let settled = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    RuntimeEvent::OperationSettled { operation_id, .. }
                        if *operation_id == accepted.operation_id
                )
            })
            .count();
        assert_eq!(settled, 1, "join settlement must appear once: {events:?}");
    })
    .await
    .expect("driver-join reserve test timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_staging_deadline_keeps_driver_committed_settlement() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let model = Arc::new(FakeModel::succeeds(&["hello"]));
        let sessions = Arc::new(MemorySessionStore::new());
        let (handle, sub) = start_with_bounds(
            sessions,
            Arc::new(SequentialIdSource::new()),
            ChannelBounds {
                command_cap: 4,
                control_cap: 8,
                event_cap: 2,
                queue_max: 4,
                driver_event_budget: 8,
                reliable_staging_cap: 4,
            },
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

        let wait_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = handle.snapshot().await;
            let stats = handle.outbound_stats().await;
            let not_yet_recorded = snapshot
                .last_settled
                .iter()
                .all(|settled| settled.operation_id != accepted.operation_id);
            let staging_full_for_producer =
                stats.reliable_staging_len + 1 >= stats.reliable_staging_cap;
            let driver_settled = snapshot.active.as_ref().is_some_and(|active| {
                matches!(
                    active.phase,
                    OperationPhase::Settled(OperationStatus::Succeeded)
                )
            });
            if not_yet_recorded && staging_full_for_producer && driver_settled {
                break;
            }
            if !not_yet_recorded {
                break;
            }
            assert!(
                tokio::time::Instant::now() < wait_deadline,
                "timed out waiting for staging pressure with a driver settlement still unpublished"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        let shutdown = handle.shutdown(
            ShutdownMode::Forced,
            Instant::now() + Duration::from_secs(2),
        );
        let resume = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            probe.resume();
        };
        let (result, _) = tokio::join!(shutdown, resume);
        let report = result.expect("shutdown must complete");
        assert!(
            report.settlements.is_empty(),
            "Committed driver settlement must not become leftover Forced Failed: {report:?}"
        );

        let events = probe
            .wait_for(
                |events| {
                    events
                        .iter()
                        .any(|event| matches!(event, RuntimeEvent::EpochEnded { .. }))
                },
                Duration::from_secs(5),
            )
            .await;
        let settled: Vec<_> = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    RuntimeEvent::OperationSettled { operation_id, .. }
                        if *operation_id == accepted.operation_id
                )
            })
            .collect();
        assert_eq!(
            settled.len(),
            1,
            "public stream must carry exactly one Committed settlement: {events:?}"
        );
        match settled[0] {
            RuntimeEvent::OperationSettled {
                status: OperationStatus::Succeeded,
                session_revision: SettlementRevision::Committed(_),
                ..
            } => {}
            other => panic!("expected Committed success, got {other:?}"),
        }
        let forced_count = events.iter().find_map(|event| match event {
            RuntimeEvent::EpochEnded { forced_count, .. } => Some(*forced_count),
            _ => None,
        });
        assert_eq!(forced_count, Some(0));
    })
    .await
    .expect("full-staging committed settlement test timed out");
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_returns_existing_completion_when_deadline_already_expired() {
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
        .shutdown(ShutdownMode::Drain, Instant::now() + Duration::from_secs(2))
        .await
        .expect("first shutdown");
    assert_eq!(first.final_state, ShutdownState::Stopped);
    let second = handle
        .shutdown(
            ShutdownMode::Forced,
            Instant::now() - Duration::from_secs(1),
        )
        .await
        .expect("already-stopped shutdown must ignore an expired deadline");
    assert_eq!(first.epoch, second.epoch);
    assert_eq!(second.final_state, ShutdownState::Stopped);
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
        .shutdown(ShutdownMode::Drain, Instant::now() + Duration::from_secs(2))
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
