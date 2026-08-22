//! INTEGRATION-005: the multi-round tool loop over the JSONL durable backend,
//! including process-restart continuation (M5-001 / M5-002).

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use philo_agent_runtime::{
    AgentFailureKind, GenerationConfig, ModelAssistantBlock, ModelMessage, ModelToolResultOutcome,
    OperationOutcome, RuntimeConfig, SequentialIdSource, SessionId, SettlementDurability,
    ToolDefinition, UserMessage, UserPart,
};
use philo_session::{ContextMessage, SessionStore, ToolResultOutcome};
use philo_session_jsonl::JsonlSessionStore;
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-m5-restart-{}-{}",
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
        recovery: Default::default(),
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

/// Sequential IDs with a distinct prefix. Operation/Turn IDs must stay unique
/// across process restarts of the same session; that uniqueness is the
/// responsibility of the injected `IdSource`, which this source simulates for
/// the restarted "process".
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

fn tool_round(call_id: &str) -> ModelScript {
    ModelScript::tool_call(0, Some(call_id), Some("echo"), &["{}"])
}

/// M5-001: the existing multi-round success trajectory holds on the JSONL
/// backend with all facts durable in source order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_round_loop_succeeds_on_the_jsonl_backend() {
    let root = TempRoot::new();
    let sessions = Arc::new(JsonlSessionStore::open(&root.path).expect("open store"));
    let model = Arc::new(FakeModel::new([
        tool_round("call-1"),
        tool_round("call-2"),
        ModelScript::text(&["final"]),
    ]));
    let tools = Arc::new(FakeTool::new(
        [echo_definition()],
        [
            FakeToolResult::success("one"),
            FakeToolResult::success("two"),
        ],
    ));
    let handle = runtime(model, sessions.clone(), tools, 2)
        .await
        .prompt(SessionId::new("m5-001"), UserMessage::new("hi"))
        .await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "final"
    ));

    let view = sessions
        .context_view(&philo_session::SessionId::new("m5-001"))
        .await
        .expect("context view");
    let kinds: Vec<&str> = view
        .messages()
        .iter()
        .map(|message| match message {
            ContextMessage::Summary { .. } => "summary",
            ContextMessage::User { .. } => "user",
            ContextMessage::AssistantToolCalls { .. } => "calls",
            ContextMessage::ToolResult { .. } => "result",
            ContextMessage::Assistant { .. } => "assistant",
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["user", "calls", "result", "calls", "result", "assistant"],
        "interleaved rounds persist in source order"
    );
}

/// M5-001 failure leg: a model failure settles Confirmed with the failure
/// facts durable on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_failure_settles_confirmed_on_the_jsonl_backend() {
    let root = TempRoot::new();
    let sessions = Arc::new(JsonlSessionStore::open(&root.path).expect("open store"));
    let model = Arc::new(FakeModel::new([ModelScript::error("provider exploded")]));
    let tools = Arc::new(FakeTool::new([echo_definition()], []));
    let agent = runtime(model, sessions.clone(), tools, 2).await;
    let handle = agent
        .prompt(SessionId::new("m5-fail"), UserMessage::new("hi"))
        .await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Failed { failure, durability }
            if failure.kind() == AgentFailureKind::ModelCall
                && durability == SettlementDurability::Confirmed
    ));

    // The failure facts survive a restart: revision covers both save points.
    drop(handle);
    agent.stop().await;
    drop(sessions);
    let reopened = Arc::new(support::jsonl::reopen(&root.path).await);
    let view = reopened
        .context_view(&philo_session::SessionId::new("m5-fail"))
        .await
        .expect("view");
    assert_eq!(view.revision(), philo_session::SessionRevision::new(2));
}

/// M5-002: after dropping the store, a fresh instance over the same root
/// rebuilds the full history and the next turn's ModelCallSnapshot replays
/// the interleaved tool trajectory unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_continues_with_the_full_replayed_history() {
    let root = TempRoot::new();
    let session = SessionId::new("m5-002");

    // Turn 1: two tool rounds, then a final answer.
    {
        let sessions = Arc::new(JsonlSessionStore::open(&root.path).expect("open store"));
        let model = Arc::new(FakeModel::new([
            tool_round("call-1"),
            tool_round("call-2"),
            ModelScript::text(&["turn one done"]),
        ]));
        let tools = Arc::new(FakeTool::new(
            [echo_definition()],
            [
                FakeToolResult::success("round one output"),
                FakeToolResult::business_error("bad_input", "round two failed"),
            ],
        ));
        let agent = runtime(model, sessions, tools, 2).await;
        let handle = agent
            .prompt(session.clone(), UserMessage::new("first prompt"))
            .await;
        assert!(matches!(
            handle.wait().await,
            OperationOutcome::Succeeded { .. }
        ));
        drop(handle);
        agent.stop().await;
        // Store dropped here: simulates the process exiting.
    }

    // Restart: a fresh store instance recovers the session from disk.
    let reopened = Arc::new(support::jsonl::reopen(&root.path).await);
    let report = reopened
        .recover_session(&philo_session::SessionId::new("m5-002"))
        .expect("recovery succeeds");
    assert_eq!(report.transactions(), 6, "A, B1, C1, B2, C2, final");
    assert!(!report.tail_was_truncated());

    let model = Arc::new(FakeModel::succeeds(&["turn two done"]));
    let tools = Arc::new(FakeTool::new([echo_definition()], []));
    let restarted = support::runtime::TestRuntime::with_tools(
        model.clone(),
        reopened.clone(),
        Arc::new(RestartIdSource {
            inner: SequentialIdSource::new(),
        }),
        config(2),
        tools,
    )
    .await;
    let handle = restarted
        .prompt(session, UserMessage::new("second prompt"))
        .await;
    let outcome = handle.wait().await;
    let OperationOutcome::Succeeded { assistant } = &outcome else {
        panic!("turn two failed: {outcome:?}");
    };
    assert_eq!(assistant.content(), "turn two done");

    // The second turn's snapshot replays the full interleaved history.
    let calls = model.calls();
    assert_eq!(calls.len(), 1);
    let messages = &calls[0].messages;
    assert_eq!(
        messages[0],
        ModelMessage::System {
            content: "sys".to_owned()
        }
    );
    assert_eq!(
        messages[1],
        ModelMessage::User {
            parts: vec![UserPart::Text("first prompt".to_owned())]
        }
    );
    let ModelMessage::Assistant { blocks: round_one } = &messages[2] else {
        panic!("expected first tool round, got {:?}", messages[2]);
    };
    let ModelAssistantBlock::ToolCall(round_one_call) = &round_one[0] else {
        panic!("expected first tool call block, got {:?}", round_one[0]);
    };
    assert_eq!(round_one_call.tool_call_id.as_str(), "call-1");
    assert_eq!(
        messages[3],
        ModelMessage::ToolResult {
            tool_call_id: philo_agent_runtime::ToolCallId::new("call-1"),
            outcome: ModelToolResultOutcome::Success {
                content: "round one output".to_owned()
            },
        }
    );
    let ModelMessage::Assistant { blocks: round_two } = &messages[4] else {
        panic!("expected second tool round, got {:?}", messages[4]);
    };
    let ModelAssistantBlock::ToolCall(round_two_call) = &round_two[0] else {
        panic!("expected second tool call block, got {:?}", round_two[0]);
    };
    assert_eq!(round_two_call.tool_call_id.as_str(), "call-2");
    assert_eq!(
        messages[5],
        ModelMessage::ToolResult {
            tool_call_id: philo_agent_runtime::ToolCallId::new("call-2"),
            outcome: ModelToolResultOutcome::Error {
                code: "bad_input".to_owned(),
                message: "round two failed".to_owned()
            },
        }
    );
    assert_eq!(
        messages[6],
        ModelMessage::Assistant {
            blocks: vec![ModelAssistantBlock::Text {
                text: "turn one done".to_owned(),
            }]
        }
    );
    assert_eq!(
        messages[7],
        ModelMessage::User {
            parts: vec![UserPart::Text("second prompt".to_owned())]
        }
    );
    assert_eq!(messages.len(), 8);

    // Durable facts across both turns stay interleaved in source order.
    let view = reopened
        .context_view(&philo_session::SessionId::new("m5-002"))
        .await
        .expect("view");
    let error_codes: Vec<&str> = view
        .messages()
        .iter()
        .filter_map(|message| match message {
            ContextMessage::ToolResult {
                outcome: ToolResultOutcome::Error { code, .. },
                ..
            } => Some(code.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(error_codes, vec!["bad_input"]);
}
