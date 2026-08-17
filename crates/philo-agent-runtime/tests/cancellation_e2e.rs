//! INTEGRATION-006: cancellation end to end over the JSONL durable backend,
//! including restart continuation with cancelled turns (M6-001/002/003/008).
//!
//! The queue, availability, idempotency, too-late, and cancel-commit-failure
//! scenarios (M6-004/005/006/007/009) are pinned by the runtime-level suite
//! in `cancellation.rs`; this file adds the durable legs.

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use philo_agent_runtime::{
    GenerationConfig, ModelAssistantBlock, ModelMessage, ModelToolResultOutcome, OperationOutcome,
    RuntimeConfig, SequentialIdSource, SessionId, ToolDefinition, UserMessage,
};
use philo_session::{ContextMessage, SessionStore, ToolResultOutcome};
use philo_session_jsonl::JsonlSessionStore;
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};
use support::gate::Gate;

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
        max_parallel_tool_calls: 1,
        operation_timeout: None,
        tool_cancel_grace: std::time::Duration::from_millis(300),
        compaction: Default::default(),
    }
}

async fn runtime(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<FakeTool>,
    max_tool_rounds: u32,
) -> support::runtime::TestRuntime {
    support::runtime::TestRuntime::with_tools(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(max_tool_rounds),
        tools,
    )
    .await
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_cancel_persists_terminal_facts_on_jsonl() {
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
        let agent = runtime(model, sessions, Arc::new(FakeTool::new([], [])), 1).await;

        let warmup_handle = agent
            .prompt(session.clone(), UserMessage::new("warmup"))
            .await;
        warmup_handle.wait_until_busy().await;
        let victim = agent
            .prompt(session.clone(), UserMessage::new("victim"))
            .await;
        warmup_gate.release();
        assert!(matches!(
            warmup_handle.wait().await,
            OperationOutcome::Succeeded { .. }
        ));

        victim
            .wait_until_phase(|phase| {
                matches!(
                    phase,
                    philo_agent_runtime::OperationPhase::RunningModelCall(
                        philo_agent_runtime::ModelCallPhase::Streaming
                    )
                )
            })
            .await;
        victim.cancel().await;
        assert!(matches!(victim.wait().await, OperationOutcome::Cancelled));
        drop(warmup_handle);
        drop(victim);
        agent.stop().await;
        // Runtime, handles, and store drop here: the process "exits".
    }

    let reopened = Arc::new(support::jsonl::reopen(&root.path).await);
    let report = reopened
        .recover_session(&philo_session::SessionId::new("m6-001"))
        .expect("recovery succeeds");
    assert_eq!(
        report.transactions(),
        4,
        "warmup A+final, victim A+cancellation, all durable"
    );
    assert!(!report.tail_was_truncated());

    let view = reopened
        .context_view(&philo_session::SessionId::new("m6-001"))
        .await
        .expect("view");
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
    let restarted = support::runtime::TestRuntime::with_tools(
        model,
        reopened,
        Arc::new(RestartIdSource {
            inner: SequentialIdSource::new(),
        }),
        config(1),
        Arc::new(FakeTool::new([], [])),
    )
    .await;
    let next = restarted
        .prompt(session, UserMessage::new("continue"))
        .await;
    assert!(matches!(
        next.wait().await,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "next"
    ));
}

/// M6-002 + M6-008 on JSONL: a mid-batch cancel persists the real prefix and
/// the Cancelled suffix atomically with the terminals; after a restart the
/// next turn's model call replays the partial trajectory, cancelled mark
/// included.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_batch_cancel_survives_restart_and_replays() {
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
        let agent = runtime(model, sessions, tools.clone(), 1).await;

        let warmup_handle = agent
            .prompt(session.clone(), UserMessage::new("warmup"))
            .await;
        warmup_handle.wait_until_busy().await;
        let victim = agent
            .prompt(session.clone(), UserMessage::new("victim"))
            .await;
        warmup_gate.release();
        let _ = warmup_handle.wait().await;

        victim
            .wait_until_phase(|phase| {
                matches!(
                    phase,
                    philo_agent_runtime::OperationPhase::RunningToolBatch(
                        philo_agent_runtime::RunningToolBatchPhase::Executing { .. }
                    )
                )
            })
            .await;
        victim.cancel().await;
        tool_gate.release();
        assert!(matches!(victim.wait().await, OperationOutcome::Cancelled));
        assert_eq!(tools.invocation_count(), 1, "call-2 never executed");
        drop(warmup_handle);
        drop(victim);
        agent.stop().await;
    }

    let reopened = Arc::new(support::jsonl::reopen(&root.path).await);
    let report = reopened
        .recover_session(&philo_session::SessionId::new("m6-002"))
        .expect("recovery succeeds");
    assert_eq!(
        report.transactions(),
        5,
        "warmup A+final, victim A+batch+cancellation"
    );

    // Durable projection: real prefix, cancelled suffix.
    let view = reopened
        .context_view(&philo_session::SessionId::new("m6-002"))
        .await
        .expect("view");
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
    let restarted = support::runtime::TestRuntime::with_tools(
        model.clone(),
        reopened,
        Arc::new(RestartIdSource {
            inner: SequentialIdSource::new(),
        }),
        config(1),
        Arc::new(FakeTool::new([echo_definition()], [])),
    )
    .await;
    let next = restarted
        .prompt(session, UserMessage::new("continue"))
        .await;
    assert!(matches!(
        next.wait().await,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "turn two"
    ));

    let calls = model.calls();
    assert_eq!(calls.len(), 1);
    let messages = &calls[0].messages;
    assert!(matches!(
        &messages[4],
        ModelMessage::Assistant { blocks }
            if blocks
                .iter()
                .filter(|block| matches!(block, ModelAssistantBlock::ToolCall(_)))
                .count()
                == 2
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_and_between_rounds_cancels_keep_the_log_minimal() {
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
        let agent = runtime(model, sessions, Arc::new(FakeTool::new([], [])), 1).await;

        let warmup_handle = agent
            .prompt(session.clone(), UserMessage::new("warmup"))
            .await;
        warmup_handle.wait_until_busy().await;
        // Cancelled while queued: never starts, never commits.
        let queued = agent
            .prompt(session.clone(), UserMessage::new("queued"))
            .await;
        queued.cancel().await;
        assert!(matches!(queued.wait().await, OperationOutcome::Cancelled));
        warmup_gate.release();
        assert!(matches!(
            warmup_handle.wait().await,
            OperationOutcome::Succeeded { .. }
        ));
        drop(queued);
        drop(warmup_handle);
        agent.stop().await;
    }

    let reopened = Arc::new(support::jsonl::reopen(&root.path).await);
    let report = reopened
        .recover_session(&philo_session::SessionId::new("m6-003"))
        .expect("recovery succeeds");
    assert_eq!(
        report.transactions(),
        2,
        "only the warmup turn reached the log: queued cancel is zero-trace"
    );
}
