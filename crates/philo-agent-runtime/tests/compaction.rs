//! RUNTIME-010: automatic and manual compaction, scheduler maintenance,
//! dual-signal policy, summary validation, and cancellation traces.

mod support;

use std::sync::Arc;

use philo_agent_runtime::{
    AgentAvailability, AgentEvent, CompactionConfig, CompactionError, CompactionReport,
    GenerationConfig, ModelMessage, OperationOutcome, OperationPhase, RuntimeConfig,
    SequentialIdSource, SessionId, TokenUsage, UserMessage,
};
use philo_session::{
    ContextMessage, MemorySessionStore, SessionEntryKind, SessionStore, SessionTransaction,
};
use support::failing_session::{FailingSessionStore, FailurePlan};
use support::fake_model::{FakeModel, ModelScript};
use support::gate::Gate;
use support::gated_session::GatedSessionStore;

fn config(compaction: CompactionConfig) -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "system".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds: 0,
        max_parallel_tool_calls: 1,
        operation_timeout: None,
        tool_cancel_grace: std::time::Duration::from_millis(300),
        compaction,
        recovery: Default::default(),
    }
}

fn manual_config(keep_recent_turns: u32) -> CompactionConfig {
    CompactionConfig {
        keep_recent_turns,
        ..CompactionConfig::default()
    }
}

fn auto_config(keep_recent_turns: u32) -> CompactionConfig {
    CompactionConfig {
        context_budget: Some(100),
        auto_threshold: 0.8,
        keep_recent_turns,
        estimate_bytes_per_token: 1_000,
    }
}

async fn runtime(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    compaction: CompactionConfig,
) -> support::runtime::TestRuntime {
    support::runtime::TestRuntime::new(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(compaction),
    )
    .await
}

fn sid() -> SessionId {
    SessionId::new("m13-runtime")
}

fn stored_sid() -> philo_session::SessionId {
    philo_session::SessionId::new(sid().as_str())
}

async fn seed_turn(store: &dyn SessionStore, index: usize, user: &str, assistant: &str) -> String {
    let session_id = stored_sid();
    let revision = store
        .context_view(&session_id)
        .await
        .expect("seed context")
        .revision();
    let operation_id = philo_session::OperationId::new(format!("seed-operation-{index}"));
    let turn_id = philo_session::TurnId::new(format!("seed-turn-{index}"));
    let commit = store
        .commit(SessionTransaction::linear(
            session_id,
            revision,
            vec![
                SessionEntryKind::OperationStarted {
                    operation_id: operation_id.clone(),
                },
                SessionEntryKind::TurnStarted {
                    operation_id: operation_id.clone(),
                    turn_id: turn_id.clone(),
                },
                SessionEntryKind::UserMessage {
                    turn_id: turn_id.clone(),
                    parts: philo_session::SessionUserPart::text_parts(user),
                },
                SessionEntryKind::AssistantMessage {
                    turn_id: turn_id.clone(),
                    blocks: vec![philo_session::SessionAssistantBlock::Text {
                        text: assistant.to_owned(),
                    }],
                },
                SessionEntryKind::TurnTerminated {
                    turn_id,
                    outcome: philo_session::TurnOutcome::Succeeded,
                },
                SessionEntryKind::OperationSettled {
                    operation_id,
                    outcome: philo_session::OperationOutcome::Succeeded,
                },
            ],
        ))
        .await
        .expect("seed turn");
    commit.current_leaf().as_str().to_owned()
}

async fn run_prompt(
    agent: &support::runtime::TestRuntime,
    text: &str,
) -> (OperationOutcome, Vec<AgentEvent>) {
    let handle = agent.prompt(sid(), UserMessage::new(text)).await;
    let mut events = Vec::new();
    while let Some(event) = handle.next_event().await {
        events.push(event);
    }
    (handle.wait().await, events)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_compaction_advances_boundaries_and_feeds_the_previous_summary() {
    let store = Arc::new(MemorySessionStore::new());
    seed_turn(store.as_ref(), 1, "u1", "a1").await;
    let second_boundary = seed_turn(store.as_ref(), 2, "u2", "a2").await;
    seed_turn(store.as_ref(), 3, "u3", "a3").await;
    let model = Arc::new(FakeModel::new([
        ModelScript::summary("summary one"),
        ModelScript::summary("summary two"),
    ]));
    let agent = runtime(model.clone(), store.clone(), manual_config(1)).await;

    assert_eq!(
        agent.compact(sid()).await.expect("first compact"),
        CompactionReport::Compacted {
            covers_up_to: second_boundary,
        }
    );
    seed_turn(store.as_ref(), 4, "u4", "a4").await;
    let before_second = store.context_view(&stored_sid()).await.expect("view");
    let third_boundary = before_second.settled_turn_boundaries()[2]
        .as_str()
        .to_owned();
    assert_eq!(
        agent.compact(sid()).await.expect("second compact"),
        CompactionReport::Compacted {
            covers_up_to: third_boundary,
        }
    );

    let calls = model.calls();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|call| call.tools.is_empty()));
    assert!(calls.iter().all(|call| {
        call.operation_id.as_str().contains("compaction")
            && call.turn_id.as_str().contains("compaction")
            && call.model_call_id.as_str().contains("compaction")
    }));
    assert!(
        calls[1].messages.iter().any(
            |message| matches!(message, ModelMessage::Summary { text } if text == "summary one")
        )
    );
    let view = store
        .context_view(&stored_sid())
        .await
        .expect("compacted view");
    assert!(matches!(
        view.messages(),
        [ContextMessage::Summary { text }, ContextMessage::User { .. }, ContextMessage::Assistant { .. }]
            if text == "summary two"
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_compaction_reports_no_work_without_calling_the_model() {
    let store = Arc::new(MemorySessionStore::new());
    seed_turn(store.as_ref(), 1, "u1", "a1").await;
    let model = Arc::new(FakeModel::new([]));
    let agent = runtime(model.clone(), store, manual_config(1)).await;

    assert_eq!(
        agent.compact(sid()).await.expect("compact result"),
        CompactionReport::NothingToCompact
    );
    assert_eq!(model.call_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn latest_real_input_usage_triggers_pre_turn_compaction_in_event_order() {
    let store = Arc::new(MemorySessionStore::new());
    seed_turn(store.as_ref(), 1, "seed", "answer").await;
    let model = Arc::new(FakeModel::new([
        ModelScript::text(&["first"]).with_usage(TokenUsage {
            input_tokens: Some(80),
            ..TokenUsage::default()
        }),
        ModelScript::summary("usage summary"),
        ModelScript::text(&["second"]),
    ]));
    let agent = runtime(model.clone(), store.clone(), auto_config(1)).await;

    assert!(matches!(
        run_prompt(&agent, "one").await.0,
        OperationOutcome::Succeeded { .. }
    ));
    let (outcome, events) = run_prompt(&agent, "two").await;
    assert!(matches!(outcome, OperationOutcome::Succeeded { .. }));
    let names = events
        .iter()
        .map(|event| match event {
            AgentEvent::OperationStarted { .. } => "operation",
            AgentEvent::ContextCompactionStarted => "compact-start",
            AgentEvent::ContextCompactionCompleted { .. } => "compact-done",
            AgentEvent::TurnStarted { .. } => "turn",
            _ => "other",
        })
        .filter(|name| *name != "other")
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        ["operation", "compact-start", "compact-done", "turn"]
    );
    assert!(model.calls()[2].messages.iter().any(
        |message| matches!(message, ModelMessage::Summary { text } if text == "usage summary")
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_without_usage_uses_context_byte_estimate() {
    let store = Arc::new(MemorySessionStore::new());
    seed_turn(store.as_ref(), 1, &"x".repeat(256), "large answer").await;
    let model = Arc::new(FakeModel::new([
        ModelScript::summary("estimated summary"),
        ModelScript::text(&["continued"]),
    ]));
    let agent = runtime(
        model.clone(),
        store,
        CompactionConfig {
            context_budget: Some(10),
            auto_threshold: 0.8,
            keep_recent_turns: 0,
            estimate_bytes_per_token: 1,
        },
    )
    .await;

    assert!(matches!(
        run_prompt(&agent, "next").await.0,
        OperationOutcome::Succeeded { .. }
    ));
    assert_eq!(model.call_count(), 2);
    assert!(matches!(
        model.calls()[1].messages[1],
        ModelMessage::Summary { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_summary_output_warns_and_does_not_block_the_turn() {
    let store = Arc::new(MemorySessionStore::new());
    seed_turn(store.as_ref(), 1, &"x".repeat(256), "a1").await;
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_calls(&[(0, "call", "tool", "{}")]),
        ModelScript::text(&["normal answer"]),
    ]));
    let agent = runtime(
        model,
        store.clone(),
        CompactionConfig {
            context_budget: Some(1),
            keep_recent_turns: 0,
            estimate_bytes_per_token: 1,
            ..CompactionConfig::default()
        },
    )
    .await;

    let (outcome, events) = run_prompt(&agent, "continue").await;
    assert!(matches!(outcome, OperationOutcome::Succeeded { .. }));
    let failed = events.iter().position(
        |event| matches!(event, AgentEvent::ContextCompactionFailed { message } if message.contains("FinalText")),
    );
    let turn_started = events
        .iter()
        .position(|event| matches!(event, AgentEvent::TurnStarted { .. }));
    assert!(failed.is_some_and(|failed| turn_started.is_some_and(|turn| failed < turn)));
    let view = store.context_view(&stored_sid()).await.expect("view");
    assert!(view.latest_compaction_boundary().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compaction_commit_failure_warns_and_the_turn_still_runs() {
    let inner = Arc::new(MemorySessionStore::new());
    seed_turn(inner.as_ref(), 1, &"x".repeat(256), "a1").await;
    let sessions = Arc::new(FailingSessionStore::around(
        inner.clone(),
        FailurePlan::commit_at(1),
    ));
    let model = Arc::new(FakeModel::new([
        ModelScript::summary("not committed"),
        ModelScript::text(&["normal answer"]),
    ]));
    let agent = runtime(
        model,
        sessions,
        CompactionConfig {
            context_budget: Some(1),
            keep_recent_turns: 0,
            estimate_bytes_per_token: 1,
            ..CompactionConfig::default()
        },
    )
    .await;

    let (outcome, events) = run_prompt(&agent, "continue").await;
    assert!(matches!(outcome, OperationOutcome::Succeeded { .. }));
    assert!(events.iter().any(
        |event| matches!(event, AgentEvent::ContextCompactionFailed { message } if message.contains("committing context compaction")),
    ));
    let view = inner.context_view(&stored_sid()).await.expect("view");
    assert!(view.latest_compaction_boundary().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maintenance_is_exclusive_and_drop_releases_the_fifo_head() {
    let store = Arc::new(MemorySessionStore::new());
    seed_turn(store.as_ref(), 1, "u1", "a1").await;
    let gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&["partial"], &gate, &["unused"]),
        ModelScript::text(&["queued answer"]),
    ]));
    let agent = runtime(model.clone(), store.clone(), manual_config(0)).await;
    let accepted = agent
        .start_compaction(sid())
        .await
        .expect("compaction accepted");
    support::runtime::wait_until_compacting(&agent.handle, &sid()).await;
    support::runtime::wait_until(|| model.call_count() >= 1).await;
    assert_eq!(
        agent.availability().await,
        AgentAvailability::Compacting { session_id: sid() }
    );
    assert!(matches!(
        agent.compact(sid()).await,
        Err(CompactionError::Unavailable {
            availability: AgentAvailability::Compacting { .. }
        })
    ));
    let handle = agent.prompt(sid(), UserMessage::new("queued")).await;
    assert_eq!(handle.phase().await, OperationPhase::Queued);

    agent.cancel_maintenance(accepted.id).await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { .. }
    ));
    let view = store.context_view(&stored_sid()).await.expect("view");
    assert!(view.latest_compaction_boundary().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_during_automatic_summary_leaves_zero_new_trace() {
    let store = Arc::new(MemorySessionStore::new());
    seed_turn(store.as_ref(), 1, &"x".repeat(256), "a1").await;
    let before = store.context_view(&stored_sid()).await.expect("before");
    let gate = Gate::new();
    let model = Arc::new(FakeModel::new([ModelScript::text_suspending(
        &["partial"],
        &gate,
        &["unused"],
    )]));
    let agent = runtime(
        model,
        store.clone(),
        CompactionConfig {
            context_budget: Some(1),
            keep_recent_turns: 0,
            estimate_bytes_per_token: 1,
            ..CompactionConfig::default()
        },
    )
    .await;
    let handle = agent.prompt(sid(), UserMessage::new("cancel")).await;
    loop {
        match handle.next_event().await {
            Some(AgentEvent::ContextCompactionStarted) => break,
            Some(AgentEvent::OperationSettled { .. }) => {
                panic!("operation settled before compaction started")
            }
            Some(_) => continue,
            None => panic!("stream ended before compaction started"),
        }
    }
    handle.cancel().await;
    assert_eq!(handle.wait().await, OperationOutcome::Cancelled);

    let after = store.context_view(&stored_sid()).await.expect("after");
    assert_eq!(after.revision(), before.revision());
    assert!(after.latest_compaction_boundary().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_after_compaction_commit_keeps_compaction_but_no_turn() {
    let inner = Arc::new(MemorySessionStore::new());
    seed_turn(inner.as_ref(), 1, &"x".repeat(256), "a1").await;
    let gate = Gate::new();
    let sessions =
        Arc::new(GatedSessionStore::around(inner.clone()).gate_after_commit_at(1, &gate));
    let model = Arc::new(FakeModel::new([ModelScript::summary("durable summary")]));
    let agent = runtime(
        model,
        sessions,
        CompactionConfig {
            context_budget: Some(1),
            keep_recent_turns: 0,
            estimate_bytes_per_token: 1,
            ..CompactionConfig::default()
        },
    )
    .await;
    let handle = agent.prompt(sid(), UserMessage::new("cancel")).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let view = inner.context_view(&stored_sid()).await.expect("view");
        if view.latest_compaction_boundary().is_some() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for compaction commit");
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    handle.cancel().await;
    gate.release();
    assert_eq!(handle.wait().await, OperationOutcome::Cancelled);

    let mut events = Vec::new();
    while let Some(event) = handle.next_event().await {
        events.push(event);
    }
    let completed = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ContextCompactionCompleted { .. }));
    let cancelled = events
        .iter()
        .position(|event| matches!(event, AgentEvent::CancellationRequested { .. }));
    assert!(completed.is_some_and(|done| cancelled.is_some_and(|cancel| done < cancel)));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnStarted { .. }))
    );
    let view = inner.context_view(&stored_sid()).await.expect("view");
    assert!(view.latest_compaction_boundary().is_some());
    assert!(matches!(
        view.messages(),
        [ContextMessage::Summary { text }] if text == "durable summary"
    ));
}
