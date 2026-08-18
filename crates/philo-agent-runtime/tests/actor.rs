//! Wave 1 actor-runtime tests from the 9.1 matrix that do not need TUI.

mod support;

use std::sync::Arc;
use std::time::Duration;

use philo_agent_runtime::{
    AdmissionError, AgentAvailability, AgentEvent, CancelResult, GenerationConfig,
    ModelCallSnapshot, ModelError, ModelEventStream, ModelPort, OperationOutcome, OperationSpec,
    OperationStatus, RuntimeConfig, RuntimeFuture, SessionId, SettlementDurability,
    SettlementRevision, UserMessage,
};
use philo_session::{ContextMessage, MemorySessionStore, SessionStore};
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};
use support::gate::Gate;
use support::runtime::{
    EventCursor, Harness, drain_until_settled, empty_tools, generation, start, submit_prompt,
    wait_until_busy, wait_until_queued, wait_until_shutdown_leaves_running,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_subscription_shuts_runtime_and_rejects_submit() {
    let model = Arc::new(FakeModel::succeeds(&["hello"]));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, sub) = start(model.clone(), sessions.clone(), empty_tools(), config()).await;
    let runtime_gen = generation(model.clone(), empty_tools(), config());
    let _accepted = submit_prompt(&handle, runtime_gen, "session", "hi").await;
    drop(sub);
    wait_until_shutdown_leaves_running(&handle).await;
    let rejected = handle
        .submit(OperationSpec {
            session_id: SessionId::new("session"),
            user_message: UserMessage::new("again"),
            generation: generation(model, empty_tools(), config()),
            service_request_id: None,
        })
        .await;
    assert!(
        matches!(
            rejected,
            Err(AdmissionError::ShuttingDown
                | AdmissionError::RuntimeStopped
                | AdmissionError::Backpressured)
        ),
        "submit after sink close must be rejected, got {rejected:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_consumer_still_persists_assistant_message() {
    let model = Arc::new(FakeModel::succeeds(&["hello"]));
    let sessions = Arc::new(MemorySessionStore::new());
    let runtime_gen = generation(model.clone(), empty_tools(), config());
    let (handle, mut consumed) =
        start(model.clone(), sessions.clone(), empty_tools(), config()).await;
    let accepted = handle
        .submit(OperationSpec {
            session_id: SessionId::new("session"),
            user_message: UserMessage::new("hi"),
            generation: runtime_gen,
            service_request_id: None,
        })
        .await
        .unwrap();
    let (_, outcome) = drain_until_settled(&mut consumed, &accepted.operation_id).await;
    assert!(matches!(
        outcome,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "hello"
    ));
    let view = sessions
        .context_view(&philo_session::SessionId::new("session"))
        .await
        .unwrap();
    assert!(
        view.messages()
            .iter()
            .any(|message| matches!(message, ContextMessage::Assistant { .. })),
        "live drain must still persist the assistant message"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_cancel_leaves_zero_session_facts() {
    let warmup_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::text(&["third"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let mut harness =
        Harness::launch_default(model, sessions.clone(), empty_tools(), config()).await;
    let warmup = harness.submit("s", "warmup").await;
    wait_until_busy(&harness.handle, &warmup.operation_id).await;
    let victim = harness.submit("s", "victim").await;
    let third = harness.submit("s", "third").await;
    wait_until_queued(&harness.handle, &victim.operation_id).await;
    assert_eq!(
        harness.handle.cancel(victim.operation_id.clone()).await,
        CancelResult::QueuedCancelled
    );
    warmup_gate.release();
    let (_, warmup_outcome) = harness.drain(&warmup.operation_id).await;
    assert!(matches!(warmup_outcome, OperationOutcome::Succeeded { .. }));
    let (victim_events, victim_outcome) = harness.drain(&victim.operation_id).await;
    assert!(matches!(victim_outcome, OperationOutcome::Cancelled));
    assert!(matches!(
        victim_events.as_slice(),
        [
            AgentEvent::OperationQueued { .. },
            AgentEvent::CancellationRequested { .. },
            AgentEvent::OperationSettled {
                status: OperationStatus::Cancelled,
                durability: SettlementDurability::Confirmed,
                session_revision: SettlementRevision::Unchanged,
                ..
            }
        ]
    ));
    let (_, third_outcome) = harness.drain(&third.operation_id).await;
    assert!(matches!(
        third_outcome,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "third"
    ));
    let view = sessions
        .context_view(&philo_session::SessionId::new("s"))
        .await
        .unwrap();
    assert_eq!(view.revision(), philo_session::SessionRevision::new(4));
    assert_eq!(view.messages().len(), 4, "two turns, user+assistant each");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generation_is_frozen_at_admission() {
    let gate = Gate::new();
    let first = Arc::new(FakeModel::new([ModelScript::text_suspending(
        &["par"],
        &gate,
        &["tial"],
    )]));
    let second = Arc::new(FakeModel::succeeds(&["other"]));
    let sessions = Arc::new(MemorySessionStore::new());
    let (handle, sub) = start(first.clone(), sessions, empty_tools(), config()).await;
    let mut cursor = EventCursor::new(sub);
    let gen_a = generation(first.clone(), empty_tools(), config());
    let accepted = submit_prompt(&handle, gen_a, "session", "hi").await;
    wait_until_busy(&handle, &accepted.operation_id).await;
    let mut changed = config();
    changed.system_prompt = "changed".to_owned();
    let _gen_b = generation(second.clone(), empty_tools(), changed);
    gate.release();
    let (_, outcome) = cursor.drain_until_settled(&accepted.operation_id).await;
    assert!(matches!(
        outcome,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "partial"
    ));
    assert_eq!(first.call_count(), 1);
    assert_eq!(second.call_count(), 0);
}

/// Panics on the first `start`, then forwards to an inner model.
///
/// The panic is raised in `ModelPort::start` on the driver poll stack so
/// `catch_unwind_async` can convert it to `DriverExit::Panicked`. A panic
/// inside a yielded model future has aborted the Windows test process.
struct PanicOnceModel {
    remaining: std::sync::atomic::AtomicBool,
    rest: Arc<FakeModel>,
}

impl ModelPort for PanicOnceModel {
    fn start<'a>(
        &'a self,
        request: ModelCallSnapshot,
    ) -> RuntimeFuture<'a, Result<Box<dyn ModelEventStream>, ModelError>> {
        if self
            .remaining
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            panic!("boom");
        }
        self.rest.start(request)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn driver_panic_settles_unconfirmed_and_starts_next_queued() {
    let panic_model = Arc::new(PanicOnceModel {
        remaining: std::sync::atomic::AtomicBool::new(true),
        rest: Arc::new(FakeModel::succeeds(&["recovered"])),
    });
    let sessions = Arc::new(MemorySessionStore::new());
    let mut harness = Harness::launch_default(panic_model, sessions, empty_tools(), config()).await;
    let first = harness.submit("panic-session", "panic").await;
    let second = harness.submit("panic-session", "next").await;
    let (_, first_outcome) =
        tokio::time::timeout(Duration::from_secs(5), harness.drain(&first.operation_id))
            .await
            .expect("driver panic should settle");
    assert!(matches!(
        first_outcome,
        OperationOutcome::Failed {
            durability: SettlementDurability::Unconfirmed,
            ..
        }
    ));
    let (_, second_outcome) = harness.drain(&second.operation_id).await;
    assert!(matches!(
        second_outcome,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "recovered"
    ));
    let snapshot = harness.handle.snapshot().await;
    assert!(snapshot.active.is_none());
    assert_eq!(snapshot.availability, AgentAvailability::Idle);
}

fn echo() -> philo_agent_runtime::ToolDefinition {
    philo_agent_runtime::ToolDefinition::simple(
        "echo",
        "echo",
        philo_agent_runtime::EffectClass::ReadOnly,
    )
}

/// Tool panic is raised on the driver poll stack (no yield). Yield-then-panic
/// inside a model future has aborted the Windows test process.
#[tokio::test(flavor = "current_thread")]
async fn tool_panic_settles_unconfirmed_and_starts_next_queued() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::text(&["recovered"]),
    ]));
    let tools = Arc::new(FakeTool::one(echo(), FakeToolResult::panics("boom")));
    let sessions = Arc::new(MemorySessionStore::new());
    let mut harness = Harness::launch_default(model, sessions, tools, config()).await;
    let first = harness.submit("panic-session", "panic").await;
    let second = harness.submit("panic-session", "next").await;
    let (_, first_outcome) =
        tokio::time::timeout(Duration::from_secs(5), harness.drain(&first.operation_id))
            .await
            .expect("tool panic should settle");
    assert!(matches!(
        first_outcome,
        OperationOutcome::Failed {
            durability: SettlementDurability::Unconfirmed,
            ..
        }
    ));
    let (_, second_outcome) = harness.drain(&second.operation_id).await;
    assert!(matches!(
        second_outcome,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "recovered"
    ));
    let snapshot = harness.handle.snapshot().await;
    assert!(snapshot.active.is_none());
    assert_eq!(snapshot.availability, AgentAvailability::Idle);
}
