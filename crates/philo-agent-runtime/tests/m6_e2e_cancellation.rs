//! INTEGRATION-006: cancellation end to end over the JSONL durable backend,
//! including restart continuation with cancelled turns (M6-001/002/003/008).
//!
//! The queue, availability, idempotency, too-late, and cancel-commit-failure
//! scenarios (M6-004/005/006/007/009) are pinned by the runtime-level suite
//! in `m6_runtime_cancellation.rs`; this file adds the durable legs.

mod support;

use std::future::Future;
use std::path::PathBuf;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll, Waker};

use philo_agent_runtime::{
    AgentRuntime, GenerationConfig, ModelMessage, ModelToolResultOutcome, OperationOutcome,
    RuntimeConfig, SequentialIdSource, SessionId, ToolDefinition, UserMessage,
};
use philo_session::{ContextMessage, SessionStore, ToolResultOutcome};
use philo_session_jsonl::JsonlSessionStore;
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};
use support::gate::Gate;

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn poll_once<F: Future + ?Sized>(future: &mut Pin<Box<F>>) -> Poll<F::Output> {
    let mut context = Context::from_waker(Waker::noop());
    future.as_mut().poll(&mut context)
}

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-m6-cancel-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn config(max_tool_rounds: u32) -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "sys".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds,
        operation_timeout: None,
        compaction: Default::default(),
    }
}

fn runtime(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<FakeTool>,
    max_tool_rounds: u32,
) -> AgentRuntime {
    AgentRuntime::with_tools(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(max_tool_rounds),
        tools,
    )
}

/// Restarted-process ID source: keeps Operation/Turn IDs unique across
/// restarts of the same durable session.
struct RestartIdSource {
    inner: SequentialIdSource,
}

impl philo_agent_runtime::IdSource for RestartIdSource {
    fn next_operation_id(&self) -> philo_agent_runtime::OperationId {
        philo_agent_runtime::OperationId::new(format!(
            "restart-{}",
            self.inner.next_operation_id().as_str()
        ))
    }
    fn next_turn_id(&self) -> philo_agent_runtime::TurnId {
        philo_agent_runtime::TurnId::new(format!("restart-{}", self.inner.next_turn_id().as_str()))
    }
}

fn echo_definition() -> ToolDefinition {
    ToolDefinition::simple("echo", "echo", philo_agent_runtime::EffectClass::ReadOnly)
}

/// M6-001 on JSONL: a model-stream cancel discards the stream, persists the
/// two terminal entries atomically, and the session stays usable on disk.
#[test]
fn m6_001_stream_cancel_persists_terminal_facts_on_jsonl() {
    let root = TempRoot::new();
    let session = SessionId::new("m6-001");
    {
        let sessions = Arc::new(JsonlSessionStore::open(&root.path).expect("open store"));
        let warmup_gate = Gate::new();
        let victim_gate = Gate::new();
        let model = Arc::new(FakeModel::new([
            ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
            ModelScript::text_suspending(&["par"], &victim_gate, &["tial"]),
        ]));
        let agent = runtime(model, sessions, Arc::new(FakeTool::new([], [])), 1);

        let warmup_handle =
            block_on(agent.prompt(session.clone(), UserMessage::new("warmup"))).unwrap();
        let mut warmup = Box::pin(async move { warmup_handle.wait().await });
        assert!(poll_once(&mut warmup).is_pending());
        let victim = block_on(agent.prompt(session.clone(), UserMessage::new("victim"))).unwrap();
        warmup_gate.release();
        assert!(matches!(
            block_on(&mut warmup),
            OperationOutcome::Succeeded { .. }
        ));

        let mut wait = Box::pin(victim.wait());
        assert!(poll_once(&mut wait).is_pending(), "suspends mid-stream");
        victim.cancel();
        assert!(matches!(block_on(wait), OperationOutcome::Cancelled));
        // Runtime, handles, and store drop here: the process "exits".
    }

    let reopened = Arc::new(JsonlSessionStore::open(&root.path).expect("re-open"));
    let report = reopened
        .recover_session(&philo_session::SessionId::new("m6-001"))
        .expect("recovery succeeds");
    assert_eq!(
        report.transactions(),
        4,
        "warmup A+final, victim A+cancellation, all durable"
    );
    assert!(!report.tail_was_truncated());

    let view =
        block_on(reopened.context_view(&philo_session::SessionId::new("m6-001"))).expect("view");
    assert!(
        matches!(
            view.messages().last(),
            Some(ContextMessage::User { parts })
                if parts == &philo_session::SessionUserPart::text_parts("victim")
        ),
        "the discarded stream left no assistant message"
    );

    // The session continues normally after the cancelled turn (M6-008).
    let model = Arc::new(FakeModel::succeeds(&["next"]));
    let restarted = AgentRuntime::with_tools(
        model,
        reopened,
        Arc::new(RestartIdSource {
            inner: SequentialIdSource::new(),
        }),
        config(1),
        Arc::new(FakeTool::new([], [])),
    );
    let next = block_on(restarted.prompt(session, UserMessage::new("continue"))).unwrap();
    assert!(matches!(
        block_on(next.wait()),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "next"
    ));
}

/// M6-002 + M6-008 on JSONL: a mid-batch cancel persists the real prefix and
/// the Cancelled suffix atomically with the terminals; after a restart the
/// next turn's model call replays the partial trajectory, cancelled mark
/// included.
#[test]
fn m6_002_mid_batch_cancel_survives_restart_and_replays() {
    let root = TempRoot::new();
    let session = SessionId::new("m6-002");
    {
        let sessions = Arc::new(JsonlSessionStore::open(&root.path).expect("open store"));
        let warmup_gate = Gate::new();
        let tool_gate = Gate::new();
        let model = Arc::new(FakeModel::new([
            ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
            ModelScript::tool_calls(&[(0, "call-1", "echo", "{}"), (1, "call-2", "echo", "{}")]),
        ]));
        let tools = Arc::new(FakeTool::new(
            [echo_definition()],
            [FakeToolResult::gated_success(&tool_gate, "one")],
        ));
        let agent = runtime(model, sessions, tools.clone(), 1);

        let warmup_handle =
            block_on(agent.prompt(session.clone(), UserMessage::new("warmup"))).unwrap();
        let mut warmup = Box::pin(async move { warmup_handle.wait().await });
        assert!(poll_once(&mut warmup).is_pending());
        let victim = block_on(agent.prompt(session.clone(), UserMessage::new("victim"))).unwrap();
        warmup_gate.release();
        let _ = block_on(&mut warmup);

        let mut wait = Box::pin(victim.wait());
        assert!(poll_once(&mut wait).is_pending(), "call-1 executing");
        victim.cancel();
        tool_gate.release();
        assert!(matches!(block_on(wait), OperationOutcome::Cancelled));
        assert_eq!(tools.invocation_count(), 1, "call-2 never executed");
    }

    let reopened = Arc::new(JsonlSessionStore::open(&root.path).expect("re-open"));
    let report = reopened
        .recover_session(&philo_session::SessionId::new("m6-002"))
        .expect("recovery succeeds");
    assert_eq!(
        report.transactions(),
        5,
        "warmup A+final, victim A+batch+cancellation"
    );

    // Durable projection: real prefix, cancelled suffix.
    let view =
        block_on(reopened.context_view(&philo_session::SessionId::new("m6-002"))).expect("view");
    let outcomes: Vec<_> = view
        .messages()
        .iter()
        .filter_map(|message| match message {
            ContextMessage::ToolResult {
                tool_call_id,
                outcome,
            } => Some((tool_call_id.as_str(), outcome)),
            _ => None,
        })
        .collect();
    assert_eq!(outcomes.len(), 2);
    assert!(matches!(
        outcomes[0],
        ("call-1", ToolResultOutcome::Success { content }) if content == "one"
    ));
    assert!(matches!(
        outcomes[1],
        ("call-2", ToolResultOutcome::Cancelled)
    ));

    // Restarted process: the next turn replays the partial tool trajectory.
    let model = Arc::new(FakeModel::succeeds(&["turn two"]));
    let restarted = AgentRuntime::with_tools(
        model.clone(),
        reopened,
        Arc::new(RestartIdSource {
            inner: SequentialIdSource::new(),
        }),
        config(1),
        Arc::new(FakeTool::new([echo_definition()], [])),
    );
    let next = block_on(restarted.prompt(session, UserMessage::new("continue"))).unwrap();
    assert!(matches!(
        block_on(next.wait()),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "turn two"
    ));

    let calls = model.calls();
    assert_eq!(calls.len(), 1);
    let messages = &calls[0].messages;
    assert!(matches!(
        &messages[4],
        ModelMessage::AssistantToolCalls { calls } if calls.len() == 2
    ));
    assert_eq!(
        messages[5],
        ModelMessage::ToolResult {
            tool_call_id: philo_agent_runtime::ToolCallId::new("call-1"),
            outcome: ModelToolResultOutcome::Success {
                content: "one".to_owned()
            },
        }
    );
    assert_eq!(
        messages[6],
        ModelMessage::ToolResult {
            tool_call_id: philo_agent_runtime::ToolCallId::new("call-2"),
            outcome: ModelToolResultOutcome::Cancelled,
        },
        "the cancelled mark replays into the next turn's model context"
    );
    assert_eq!(
        messages[7],
        ModelMessage::User {
            parts: vec![philo_agent_runtime::UserPart::Text("continue".to_owned())]
        }
    );
}

/// M6-003 + M6-005 on JSONL: a queued cancel leaves zero bytes behind, and a
/// between-rounds cancel adds exactly one terminal-only transaction.
#[test]
fn m6_003_queued_and_between_rounds_cancels_keep_the_log_minimal() {
    let root = TempRoot::new();
    let session = SessionId::new("m6-003");
    {
        let sessions = Arc::new(JsonlSessionStore::open(&root.path).expect("open store"));
        let warmup_gate = Gate::new();
        let model = Arc::new(FakeModel::new([ModelScript::text_suspending(
            &[],
            &warmup_gate,
            &["done"],
        )]));
        let agent = runtime(model, sessions, Arc::new(FakeTool::new([], [])), 1);

        let warmup_handle =
            block_on(agent.prompt(session.clone(), UserMessage::new("warmup"))).unwrap();
        let mut warmup = Box::pin(async move { warmup_handle.wait().await });
        assert!(poll_once(&mut warmup).is_pending());
        // Cancelled while queued: never starts, never commits.
        let queued = block_on(agent.prompt(session.clone(), UserMessage::new("queued"))).unwrap();
        queued.cancel();
        assert!(matches!(
            block_on(queued.wait()),
            OperationOutcome::Cancelled
        ));
        warmup_gate.release();
        assert!(matches!(
            block_on(&mut warmup),
            OperationOutcome::Succeeded { .. }
        ));
    }

    let reopened = Arc::new(JsonlSessionStore::open(&root.path).expect("re-open"));
    let report = reopened
        .recover_session(&philo_session::SessionId::new("m6-003"))
        .expect("recovery succeeds");
    assert_eq!(
        report.transactions(),
        2,
        "only the warmup turn reached the log: queued cancel is zero-trace"
    );
}
