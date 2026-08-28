//! Epoch supervisor: coordinator panic settles every accepted operation.

mod support;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use philo_agent_runtime::{
    AgentAvailability, AgentEvent, CompactionConfig, CompactionSpec, DiagnosticId,
    ForcedSettlement, GenerationConfig, OperationAccepted, OperationId, OperationStatus,
    RuntimeConfig, RuntimeEvent, RuntimeEventReceiver, SequentialIdSource, SessionId,
    SettlementDurability, SettlementRevision, ShutdownState,
};
use philo_session::{MemorySessionStore, SessionEntryKind, SessionStore, SessionTransaction};
use support::fake_model::{FakeModel, ModelScript};
use support::gate::Gate;
use support::runtime::{
    empty_tools, event_cap_bounds, generation, start_with_bounds, submit_prompt, wait_until_busy,
    wait_until_compacting, wait_until_idle, wait_until_queued,
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
        compaction: CompactionConfig::default(),
        recovery: Default::default(),
    }
}

fn compaction_config() -> RuntimeConfig {
    RuntimeConfig {
        compaction: CompactionConfig {
            keep_recent_turns: 1,
            ..CompactionConfig::default()
        },
        ..config()
    }
}

async fn recv_until_epoch_ended(
    sub: &mut RuntimeEventReceiver,
) -> (Vec<RuntimeEvent>, Vec<ForcedSettlement>) {
    let mut events = Vec::new();
    let mut settled = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("timed out waiting for EpochEnded")
            .expect("subscription closed before EpochEnded");
        let forced_count = match &event {
            RuntimeEvent::OperationSettled {
                operation_id,
                session_id,
                status,
                durability,
                ..
            } => {
                settled.push(ForcedSettlement {
                    operation_id: operation_id.clone(),
                    session_id: session_id.clone(),
                    status: *status,
                    durability: *durability,
                    diagnostic_id: DiagnosticId::new("from-operation-settled"),
                });
                None
            }
            RuntimeEvent::EpochEnded { forced_count, .. } => Some(*forced_count),
            _ => None,
        };
        events.push(event);
        if let Some(forced_count) = forced_count {
            let start = settled.len().saturating_sub(forced_count);
            return (events, settled.split_off(start));
        }
    }
}

fn settled_ids(
    events: &[RuntimeEvent],
) -> Vec<(OperationId, OperationStatus, SettlementDurability)> {
    events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::OperationSettled {
                operation_id,
                status,
                durability,
                ..
            }
            | RuntimeEvent::Agent(AgentEvent::OperationSettled {
                operation_id,
                status,
                durability,
                ..
            }) => Some((operation_id.clone(), *status, *durability)),
            _ => None,
        })
        .collect()
}

fn assert_exactly_one_forced_terminal(
    accepted: &[OperationAccepted],
    events: &[RuntimeEvent],
    settlements: &[ForcedSettlement],
) {
    let accepted_ids: HashSet<_> = accepted
        .iter()
        .map(|item| item.operation_id.clone())
        .collect();
    let settled = settled_ids(events);
    for id in &accepted_ids {
        let matches: Vec<_> = settled
            .iter()
            .filter(|(operation_id, _, _)| operation_id == id)
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "operation {id} should have exactly one OperationSettled, got {matches:?}"
        );
        assert_eq!(matches[0].1, OperationStatus::Failed);
        assert_eq!(matches[0].2, SettlementDurability::Unconfirmed);
    }
    let forced_events: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::OperationSettled {
                operation_id,
                session_id,
                status: OperationStatus::Failed,
                durability: SettlementDurability::Unconfirmed,
                session_revision: SettlementRevision::Unchanged,
            } if accepted_ids.contains(operation_id) => Some((operation_id, session_id)),
            _ => None,
        })
        .collect();
    assert_eq!(forced_events.len(), accepted_ids.len());
    for (_, session_id) in &forced_events {
        assert!(
            !session_id.as_str().is_empty(),
            "forced OperationSettled must carry session_id"
        );
    }
    let forced: HashSet<_> = settlements
        .iter()
        .map(|item| item.operation_id.clone())
        .collect();
    assert_eq!(forced, accepted_ids);
    for settlement in settlements {
        assert_eq!(settlement.status, OperationStatus::Failed);
        assert_eq!(settlement.durability, SettlementDurability::Unconfirmed);
        assert!(
            !settlement.session_id.as_str().is_empty(),
            "forced settlement must carry session_id"
        );
    }
    let forced_count = events
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::EpochEnded { forced_count, .. } => Some(*forced_count),
            _ => None,
        })
        .expect("EpochEnded");
    assert_eq!(forced_count, settlements.len());
    assert_eq!(forced_count, forced_events.len());
}

async fn seed_turn(store: &dyn SessionStore, index: usize) {
    let session_id = philo_session::SessionId::new("compact-session");
    let revision = store
        .context_view(&session_id)
        .await
        .expect("seed context")
        .revision();
    store
        .commit(SessionTransaction::linear(
            session_id,
            revision,
            vec![
                SessionEntryKind::OperationStarted {
                    operation_id: philo_session::OperationId::new(format!("seed-op-{index}")),
                },
                SessionEntryKind::TurnStarted {
                    operation_id: philo_session::OperationId::new(format!("seed-op-{index}")),
                    turn_id: philo_session::TurnId::new(format!("seed-turn-{index}")),
                },
                SessionEntryKind::UserMessage {
                    turn_id: philo_session::TurnId::new(format!("seed-turn-{index}")),
                    parts: philo_session::SessionUserPart::text_parts("u"),
                },
                SessionEntryKind::AssistantMessage {
                    turn_id: philo_session::TurnId::new(format!("seed-turn-{index}")),
                    blocks: vec![philo_session::SessionAssistantBlock::Text {
                        text: "a".to_owned(),
                    }],
                },
                SessionEntryKind::TurnTerminated {
                    turn_id: philo_session::TurnId::new(format!("seed-turn-{index}")),
                    outcome: philo_session::TurnOutcome::Succeeded,
                },
                SessionEntryKind::OperationSettled {
                    operation_id: philo_session::OperationId::new(format!("seed-op-{index}")),
                    outcome: philo_session::OperationOutcome::Succeeded,
                    usage: None,
                },
            ],
        ))
        .await
        .expect("seed turn");
}

#[tokio::test(flavor = "current_thread")]
async fn coordinator_panic_with_only_active_operation() {
    let gate = Gate::new();
    let model = Arc::new(FakeModel::new([ModelScript::text_suspending(
        &[],
        &gate,
        &["done"],
    )]));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, mut sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        event_cap_bounds(32),
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
    handle.inject_coordinator_panic().await;
    let (events, settlements) = recv_until_epoch_ended(&mut sub).await;
    assert_exactly_one_forced_terminal(std::slice::from_ref(&accepted), &events, &settlements);
    let snapshot = handle.snapshot().await;
    assert_eq!(snapshot.availability, AgentAvailability::Idle);
    assert!(snapshot.active.is_none());
    assert!(snapshot.queued.is_empty());
    assert_eq!(snapshot.shutdown, ShutdownState::Stopped);
    gate.release();
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    let extra = settled_ids(&events)
        .into_iter()
        .filter(|(id, _, _)| *id == accepted.operation_id)
        .count();
    assert_eq!(extra, 1);
    assert_eq!(
        handle.snapshot().await.availability,
        AgentAvailability::Idle
    );
}

#[tokio::test(flavor = "current_thread")]
async fn coordinator_panic_with_active_and_queued_operations() {
    let gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &gate, &["one"]),
        ModelScript::text(&["two"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, mut sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        event_cap_bounds(32),
    )
    .await;
    let runtime_gen = generation(model, empty_tools(), config());
    let active = submit_prompt(&handle, runtime_gen.clone(), "session", "first").await;
    wait_until_busy(&handle, &active.operation_id).await;
    let queued = submit_prompt(&handle, runtime_gen, "session", "second").await;
    wait_until_queued(&handle, &queued.operation_id).await;
    let queued_snapshot = handle.snapshot().await;
    assert_eq!(queued_snapshot.queued.len(), 1);
    assert_eq!(queued_snapshot.queued[0].operation_id, queued.operation_id);
    assert_eq!(
        queued_snapshot.queued[0].session_id,
        SessionId::new("session")
    );
    handle.inject_coordinator_panic().await;
    let (events, settlements) = recv_until_epoch_ended(&mut sub).await;
    assert_exactly_one_forced_terminal(&[active, queued], &events, &settlements);
    let snapshot = handle.snapshot().await;
    assert_eq!(snapshot.availability, AgentAvailability::Idle);
    assert!(snapshot.active.is_none());
    assert!(snapshot.queued.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn coordinator_panic_with_only_queued_operations() {
    let gate = Gate::new();
    let model = Arc::new(FakeModel::new([ModelScript::text_suspending(
        &[],
        &gate,
        &["summary"],
    )]));
    let sessions = Arc::new(MemorySessionStore::new());
    seed_turn(sessions.as_ref(), 1).await;
    seed_turn(sessions.as_ref(), 2).await;
    seed_turn(sessions.as_ref(), 3).await;
    let (handle, mut sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        event_cap_bounds(32),
    )
    .await;
    let runtime_gen = generation(model, empty_tools(), compaction_config());
    handle
        .start_compaction(CompactionSpec {
            session_id: SessionId::new("compact-session"),
            generation: runtime_gen.clone(),
        })
        .await
        .expect("start compaction");
    wait_until_compacting(&handle, &SessionId::new("compact-session")).await;
    let first = submit_prompt(&handle, runtime_gen.clone(), "compact-session", "queued-a").await;
    let second = submit_prompt(&handle, runtime_gen, "compact-session", "queued-b").await;
    wait_until_queued(&handle, &first.operation_id).await;
    wait_until_queued(&handle, &second.operation_id).await;
    assert!(handle.snapshot().await.active.is_none());
    handle.inject_coordinator_panic().await;
    let (events, settlements) = recv_until_epoch_ended(&mut sub).await;
    assert_exactly_one_forced_terminal(&[first, second], &events, &settlements);
    assert_eq!(
        handle.snapshot().await.availability,
        AgentAvailability::Idle
    );
}

#[tokio::test(flavor = "current_thread")]
async fn epoch_ended_arrives_when_event_cap_is_one() {
    let gate = Gate::new();
    let model = Arc::new(FakeModel::new([ModelScript::text_suspending(
        &["head"],
        &gate,
        &["tail"],
    )]));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, mut sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        event_cap_bounds(1),
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
    handle.inject_coordinator_panic().await;
    let (events, settlements) = recv_until_epoch_ended(&mut sub).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::EpochEnded { .. }))
    );
    assert_eq!(settlements.len(), 1);
    assert_eq!(settlements[0].operation_id, accepted.operation_id);
    assert_eq!(
        handle.snapshot().await.availability,
        AgentAvailability::Idle
    );
}

#[tokio::test(flavor = "current_thread")]
async fn settled_operation_is_not_force_settled_again() {
    let gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text(&["done"]),
        ModelScript::text_suspending(&[], &gate, &["queued"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, mut sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        event_cap_bounds(32),
    )
    .await;
    let runtime_gen = generation(model, empty_tools(), config());
    let first = submit_prompt(&handle, runtime_gen.clone(), "session", "first").await;
    wait_until_idle(&handle).await;
    let second = submit_prompt(&handle, runtime_gen, "session", "second").await;
    wait_until_busy(&handle, &second.operation_id).await;
    handle.inject_coordinator_panic().await;
    let (events, settlements) = recv_until_epoch_ended(&mut sub).await;
    let first_settled: Vec<_> = settled_ids(&events)
        .into_iter()
        .filter(|(id, _, _)| *id == first.operation_id)
        .collect();
    assert_eq!(first_settled.len(), 1);
    assert_eq!(first_settled[0].1, OperationStatus::Succeeded);
    assert!(
        settlements
            .iter()
            .all(|item| item.operation_id != first.operation_id)
    );
    assert_eq!(settlements.len(), 1);
    assert_eq!(settlements[0].operation_id, second.operation_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graceful_shutdown_still_emits_epoch_ended() {
    let model = Arc::new(FakeModel::succeeds(&["hello"]));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, mut sub) = start_with_bounds(
        sessions,
        Arc::new(SequentialIdSource::new()),
        event_cap_bounds(32),
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
    let report = handle
        .shutdown(
            philo_agent_runtime::ShutdownMode::Drain,
            Instant::now() + Duration::from_secs(30),
        )
        .await
        .expect("shutdown");
    assert_eq!(report.final_state, ShutdownState::Stopped);
    assert!(report.settlements.is_empty());
    let (_, settlements) = recv_until_epoch_ended(&mut sub).await;
    assert!(settlements.is_empty());
    let _ = accepted;
}
