//! RUNTIME-010: automatic and manual compaction, scheduler maintenance,
//! dual-signal policy, summary validation, and cancellation traces.

mod support;

use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use philo_agent_runtime::{
    AgentAvailability, AgentEvent, AgentRuntime, CompactionConfig, CompactionError,
    CompactionReport, GenerationConfig, ModelMessage, OperationOutcome, OperationPhase,
    RuntimeConfig, SequentialIdSource, SessionId, TokenUsage, UserMessage,
};
use philo_session::{
    ContextMessage, MemorySessionStore, SessionEntryKind, SessionStore, SessionTransaction,
};
use support::failing_session::{FailingSessionStore, FailurePlan};
use support::fake_model::{FakeModel, ModelScript};
use support::gate::Gate;
use support::gated_session::GatedSessionStore;

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn poll_once<F: Future + ?Sized>(future: &mut Pin<Box<F>>) -> Poll<F::Output> {
    let mut context = Context::from_waker(Waker::noop());
    future.as_mut().poll(&mut context)
}

#[derive(Default)]
struct WakeCounter(AtomicUsize);

impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn config(compaction: CompactionConfig) -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "system".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds: 0,
        max_parallel_tool_calls: 1,
        operation_timeout: None,
        compaction,
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

fn runtime(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    compaction: CompactionConfig,
) -> AgentRuntime {
    AgentRuntime::new(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(compaction),
    )
}

fn sid() -> SessionId {
    SessionId::new("m13-runtime")
}

fn stored_sid() -> philo_session::SessionId {
    philo_session::SessionId::new(sid().as_str())
}

fn seed_turn(store: &dyn SessionStore, index: usize, user: &str, assistant: &str) -> String {
    let session_id = stored_sid();
    let revision = block_on(store.context_view(&session_id))
        .expect("seed context")
        .revision();
    let operation_id = philo_session::OperationId::new(format!("seed-operation-{index}"));
    let turn_id = philo_session::TurnId::new(format!("seed-turn-{index}"));
    let commit = block_on(store.commit(SessionTransaction::linear(
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
                content: assistant.to_owned(),
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
    )))
    .expect("seed turn");
    commit.current_leaf().as_str().to_owned()
}

fn run_prompt(agent: &AgentRuntime, text: &str) -> (OperationOutcome, Vec<AgentEvent>) {
    let mut handle = block_on(agent.prompt(sid(), UserMessage::new(text))).expect("prompt");
    let mut events = Vec::new();
    while let Some(event) = block_on(handle.next_event()) {
        events.push(event);
    }
    (block_on(handle.wait()), events)
}

#[test]
fn manual_compaction_advances_boundaries_and_feeds_the_previous_summary() {
    let store = Arc::new(MemorySessionStore::new());
    seed_turn(store.as_ref(), 1, "u1", "a1");
    let second_boundary = seed_turn(store.as_ref(), 2, "u2", "a2");
    seed_turn(store.as_ref(), 3, "u3", "a3");
    let model = Arc::new(FakeModel::new([
        ModelScript::summary("summary one"),
        ModelScript::summary("summary two"),
    ]));
    let agent = runtime(model.clone(), store.clone(), manual_config(1));

    assert_eq!(
        block_on(agent.compact(sid())).expect("first compact"),
        CompactionReport::Compacted {
            covers_up_to: second_boundary,
        }
    );
    seed_turn(store.as_ref(), 4, "u4", "a4");
    let before_second = block_on(store.context_view(&stored_sid())).expect("view");
    let third_boundary = before_second.settled_turn_boundaries()[2]
        .as_str()
        .to_owned();
    assert_eq!(
        block_on(agent.compact(sid())).expect("second compact"),
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
    let view = block_on(store.context_view(&stored_sid())).expect("compacted view");
    assert!(matches!(
        view.messages(),
        [ContextMessage::Summary { text }, ContextMessage::User { .. }, ContextMessage::Assistant { .. }]
            if text == "summary two"
    ));
}

#[test]
fn manual_compaction_reports_no_work_without_calling_the_model() {
    let store = Arc::new(MemorySessionStore::new());
    seed_turn(store.as_ref(), 1, "u1", "a1");
    let model = Arc::new(FakeModel::new([]));
    let agent = runtime(model.clone(), store, manual_config(1));

    assert_eq!(
        block_on(agent.compact(sid())).expect("compact result"),
        CompactionReport::NothingToCompact
    );
    assert_eq!(model.call_count(), 0);
}

#[test]
fn latest_real_input_usage_triggers_pre_turn_compaction_in_event_order() {
    let store = Arc::new(MemorySessionStore::new());
    seed_turn(store.as_ref(), 1, "seed", "answer");
    let model = Arc::new(FakeModel::new([
        ModelScript::text(&["first"]).with_usage(TokenUsage {
            input_tokens: Some(80),
            ..TokenUsage::default()
        }),
        ModelScript::summary("usage summary"),
        ModelScript::text(&["second"]),
    ]));
    let agent = runtime(model.clone(), store.clone(), auto_config(1));

    assert!(matches!(
        run_prompt(&agent, "one").0,
        OperationOutcome::Succeeded { .. }
    ));
    let (outcome, events) = run_prompt(&agent, "two");
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

#[test]
fn restart_without_usage_uses_context_byte_estimate() {
    let store = Arc::new(MemorySessionStore::new());
    seed_turn(store.as_ref(), 1, &"x".repeat(256), "large answer");
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
    );

    assert!(matches!(
        run_prompt(&agent, "next").0,
        OperationOutcome::Succeeded { .. }
    ));
    assert_eq!(model.call_count(), 2);
    assert!(matches!(
        model.calls()[1].messages[1],
        ModelMessage::Summary { .. }
    ));
}

#[test]
fn invalid_summary_output_warns_and_does_not_block_the_turn() {
    let store = Arc::new(MemorySessionStore::new());
    seed_turn(store.as_ref(), 1, &"x".repeat(256), "a1");
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
    );

    let (outcome, events) = run_prompt(&agent, "continue");
    assert!(matches!(outcome, OperationOutcome::Succeeded { .. }));
    let failed = events.iter().position(
        |event| matches!(event, AgentEvent::ContextCompactionFailed { message } if message.contains("FinalText")),
    );
    let turn_started = events
        .iter()
        .position(|event| matches!(event, AgentEvent::TurnStarted { .. }));
    assert!(failed.is_some_and(|failed| turn_started.is_some_and(|turn| failed < turn)));
    let view = block_on(store.context_view(&stored_sid())).expect("view");
    assert!(view.latest_compaction_boundary().is_none());
}

#[test]
fn compaction_commit_failure_warns_and_the_turn_still_runs() {
    let inner = Arc::new(MemorySessionStore::new());
    seed_turn(inner.as_ref(), 1, &"x".repeat(256), "a1");
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
    );

    let (outcome, events) = run_prompt(&agent, "continue");
    assert!(matches!(outcome, OperationOutcome::Succeeded { .. }));
    assert!(events.iter().any(
        |event| matches!(event, AgentEvent::ContextCompactionFailed { message } if message.contains("committing context compaction")),
    ));
    let view = block_on(inner.context_view(&stored_sid())).expect("view");
    assert!(view.latest_compaction_boundary().is_none());
}

#[test]
fn maintenance_is_exclusive_and_drop_releases_the_fifo_head() {
    let store = Arc::new(MemorySessionStore::new());
    seed_turn(store.as_ref(), 1, "u1", "a1");
    let gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&["partial"], &gate, &["unused"]),
        ModelScript::text(&["queued answer"]),
    ]));
    let agent = runtime(model, store.clone(), manual_config(0));
    let mut compact = Box::pin(agent.compact(sid()));
    assert!(poll_once(&mut compact).is_pending());
    assert_eq!(
        agent.availability(),
        AgentAvailability::Compacting { session_id: sid() }
    );
    assert!(matches!(
        block_on(agent.compact(sid())),
        Err(CompactionError::Unavailable {
            availability: AgentAvailability::Compacting { .. }
        })
    ));
    let handle = block_on(agent.prompt(sid(), UserMessage::new("queued"))).expect("prompt");
    assert_eq!(handle.phase(), OperationPhase::Queued);
    let mut wait = Box::pin(handle.wait());
    let wakes = Arc::new(WakeCounter::default());
    let waker = Waker::from(wakes.clone());
    let mut context = Context::from_waker(&waker);
    assert!(wait.as_mut().poll(&mut context).is_pending());

    drop(compact);
    assert!(wakes.0.load(Ordering::SeqCst) > 0);
    assert!(matches!(block_on(wait), OperationOutcome::Succeeded { .. }));
    let view = block_on(store.context_view(&stored_sid())).expect("view");
    assert!(view.latest_compaction_boundary().is_none());
}

#[test]
fn cancelling_during_automatic_summary_leaves_zero_new_trace() {
    let store = Arc::new(MemorySessionStore::new());
    seed_turn(store.as_ref(), 1, &"x".repeat(256), "a1");
    let before = block_on(store.context_view(&stored_sid())).expect("before");
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
    );
    let handle = block_on(agent.prompt(sid(), UserMessage::new("cancel"))).expect("prompt");
    let mut wait = Box::pin(handle.wait());
    assert!(poll_once(&mut wait).is_pending());
    handle.cancel();
    assert_eq!(block_on(wait), OperationOutcome::Cancelled);

    let after = block_on(store.context_view(&stored_sid())).expect("after");
    assert_eq!(after.revision(), before.revision());
    assert!(after.latest_compaction_boundary().is_none());
}

#[test]
fn cancellation_after_compaction_commit_keeps_compaction_but_no_turn() {
    let inner = Arc::new(MemorySessionStore::new());
    seed_turn(inner.as_ref(), 1, &"x".repeat(256), "a1");
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
    );
    let mut handle = block_on(agent.prompt(sid(), UserMessage::new("cancel"))).expect("prompt");
    let mut wait = Box::pin(handle.wait());
    assert!(poll_once(&mut wait).is_pending());
    handle.cancel();
    gate.release();
    assert_eq!(block_on(wait), OperationOutcome::Cancelled);

    let mut events = Vec::new();
    while let Some(event) = block_on(handle.next_event()) {
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
    let view = block_on(inner.context_view(&stored_sid())).expect("view");
    assert!(view.latest_compaction_boundary().is_some());
    assert!(matches!(
        view.messages(),
        [ContextMessage::Summary { text }] if text == "durable summary"
    ));
}
