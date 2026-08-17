//! INTEGRATION-010: display-channel transience on the durable backend
//! (M10-002) and the external approval decorator sample (M10-007).

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use philo_agent_runtime::{
    EffectClass, GenerationConfig, OperationOutcome, RichToolResult, RuntimeConfig,
    SequentialIdSource, SessionId, ToolDefinition, ToolDisplay, ToolFuture, ToolInvocation,
    ToolInvokeCx, ToolInvokeEnd, ToolPort, UserMessage,
};
use philo_session::{SessionStore, ToolResultOutcome};
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
            "philo-m10-dual-{}-{}",
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

/// M10-002 on the durable backend: the display detail travels on events but
/// leaves zero bytes in the JSONL log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn display_leaves_no_trace_in_the_jsonl_log() {
    let root = TempRoot::new();
    let sessions = Arc::new(JsonlSessionStore::open(&root.path).expect("open store"));
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
        ModelScript::text(&["done"]),
    ]));
    let secret_detail = "DISPLAY-ONLY-SECRET-DETAIL-7Q";
    let tools = Arc::new(FakeTool::one(
        ToolDefinition::simple("echo", "echo", EffectClass::ReadOnly),
        FakeToolResult::success_with_display(
            "model view",
            ToolDisplay::new(secret_detail).with_fact("marker", secret_detail),
        ),
    ));
    let agent = support::runtime::TestRuntime::with_tools(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(1),
        tools,
    )
    .await;

    let handle = agent
        .prompt(SessionId::new("m10-002"), UserMessage::new("hi"))
        .await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { .. }
    ));

    // The display is visible on the event...
    let mut saw_display = false;
    while let Some(event) = handle.next_event().await {
        if let philo_agent_runtime::AgentEvent::ToolExecutionCompleted { display, .. } = event {
            saw_display = display.is_some_and(|d| d.detail() == secret_detail);
        }
    }
    assert!(saw_display, "display travels on the completion event");

    // ...and absent from every durable byte.
    let log =
        std::fs::read_to_string(root.path.join("s-m10-002").join("log.jsonl")).expect("read log");
    assert!(
        !log.contains(secret_detail),
        "display content must never be persisted"
    );
    assert!(
        log.contains("model view"),
        "the model channel is the durable fact"
    );
}

/// External approval decorator: policy lives entirely in the caller's
/// ToolPort wrapper, denial is a plain business error, and the kernel and
/// session observe nothing new (M10-007).
struct DenySystemTools<P> {
    inner: P,
}

impl<P: ToolPort> ToolPort for DenySystemTools<P> {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner.definitions()
    }
    fn invoke<'a>(&'a self, invocation: ToolInvocation, cx: ToolInvokeCx) -> ToolFuture<'a> {
        let class = self
            .definitions()
            .iter()
            .find(|definition| definition.name() == invocation.name())
            .map(ToolDefinition::effect_class);
        Box::pin(async move {
            if class == Some(EffectClass::System) {
                return Ok(ToolInvokeEnd::Done(RichToolResult::error(
                    "denied",
                    "the approval policy declined this system command",
                )));
            }
            self.inner.invoke(invocation, cx).await
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deny_decorator_rejects_system_tools_through_plain_error_semantics() {
    let model = Arc::new(FakeModel::new([
        ModelScript::tool_calls(&[(0, "call-1", "shell", r#"{"command":"rm -rf /"}"#)]),
        ModelScript::text(&["understood, not running that"]),
    ]));
    let sessions = Arc::new(philo_session::MemorySessionStore::new());
    let inner = FakeTool::new(
        [
            ToolDefinition::simple("echo", "echo", EffectClass::ReadOnly),
            ToolDefinition::simple("shell", "runs commands", EffectClass::System),
        ],
        [FakeToolResult::success("never reached")],
    );
    let agent = support::runtime::TestRuntime::with_tools(
        model,
        sessions.clone(),
        Arc::new(SequentialIdSource::new()),
        config(1),
        Arc::new(DenySystemTools { inner }),
    )
    .await;

    let handle = agent
        .prompt(SessionId::new("m10-007"), UserMessage::new("clean up"))
        .await;
    // The loop continues past the denial to a normal final answer.
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { assistant }
            if assistant.content() == "understood, not running that"
    ));

    // The denial is an ordinary durable business error; no new entry kinds,
    // no kernel/session awareness of "approval".
    let view = sessions
        .context_view(&philo_session::SessionId::new("m10-007"))
        .await
        .unwrap();
    let durable = view
        .messages()
        .iter()
        .find_map(|message| match message {
            philo_session::ContextMessage::ToolResult { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .expect("durable tool result");
    assert_eq!(
        durable,
        ToolResultOutcome::Error {
            code: "denied".to_owned(),
            message: "the approval policy declined this system command".to_owned(),
        }
    );
}
