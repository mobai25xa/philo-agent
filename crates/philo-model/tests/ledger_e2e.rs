//! INTEGRATION-010: the ledger-consistency golden. The tool's
//! finalized (truncated) model-channel text, the durable session fact, and
//! the tool-result text in the next request body are byte-for-byte equal.

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use philo_agent_runtime::{
    AgentRuntime, ChannelBounds, GenerationConfig, OperationOutcome, OperationSpec, RuntimeConfig,
    RuntimeDeps, SequentialIdSource, SessionId, ToolArguments, ToolHandler, ToolRegistry,
    UserMessage,
};
use philo_session::{ContextMessage, MemorySessionStore, SessionStore, ToolResultOutcome};
use philo_tools_std::ReadTool;
use support::{
    StubResponse, StubTransport, adapter_over, drain_until_settled, generation, sse, text_sse,
};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-m10-e2e-{}-{}",
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

fn tool_round_sse(response_id: &str, call_id: &str, path: &str) -> Vec<u8> {
    let head = format!(
        r#"{{"id":"{response_id}","object":"chat.completion.chunk","model":"stub-gpt","choices":[{{"index":0,"delta":{{"role":"assistant"}},"finish_reason":null}}]}}"#
    );
    let call = format!(
        r#"{{"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"{call_id}","type":"function","function":{{"name":"read","arguments":"{{\"path\":\"{path}\"}}"}}}}]}},"finish_reason":null}}]}}"#
    );
    let finish = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#.to_owned();
    sse(&[&head, &call, &finish, "[DONE]"])
}

/// The three-way ledger check: tool output = durable fact = next request.
#[tokio::test(flavor = "multi_thread")]
async fn truncated_tool_output_is_the_single_source_of_truth() {
    let root = TempRoot::new();
    // Content that will truncate under a tight byte limit.
    std::fs::write(root.path.join("big.txt"), "0123456789\n".repeat(50)).expect("fixture");

    // The exact expected model-channel text comes from the tool itself:
    // read is deterministic, so a direct handler call pins the golden value.
    let probe = ReadTool::new(&root.path).with_max_bytes(60);
    let expected = probe
        .call(ToolArguments::parse(r#"{"path":"big.txt"}"#).expect("valid arguments"))
        .await;
    let expected_text = expected
        .result()
        .content()
        .expect("read succeeds")
        .to_owned();
    assert!(
        expected_text.contains("[read truncated"),
        "the fixture must actually truncate: {expected_text}"
    );

    let transport = StubTransport::new([
        StubResponse::Sse(tool_round_sse("resp-1", "call-1", "big.txt")),
        StubResponse::Sse(text_sse("resp-2", "stub-gpt", &["done"])),
    ]);
    let sessions = Arc::new(MemorySessionStore::new());
    let registry = ToolRegistry::builder()
        .register(
            ReadTool::definition(),
            ReadTool::new(&root.path).with_max_bytes(60),
        )
        .expect("register read tool")
        .build();
    let generation = generation(
        Arc::new(adapter_over(transport.clone())),
        Arc::new(registry),
        RuntimeConfig {
            system_prompt: "sys".to_owned(),
            model_target: "stub-model".to_owned(),
            generation: GenerationConfig {
                max_output_tokens: 256,
                temperature: 0.0,
                reasoning_effort: None,
                tool_choice: philo_agent_runtime::ToolChoice::Auto,
            },
            max_tool_rounds: 1,
            max_parallel_tool_calls: 1,
            operation_timeout: None,
            tool_cancel_grace: std::time::Duration::from_millis(300),
            compaction: Default::default(),
        },
    );
    let (handle, mut sub) = AgentRuntime::start(RuntimeDeps {
        sessions: sessions.clone(),
        ids: Arc::new(SequentialIdSource::new()),
        bounds: ChannelBounds::default(),
    })
    .expect("start runtime");

    let accepted = handle
        .submit(OperationSpec {
            session_id: SessionId::new("m10-001"),
            user_message: UserMessage::new("read big.txt"),
            generation,
            service_request_id: None,
        })
        .await
        .expect("submit accepted");
    let (events, outcome) = drain_until_settled(&mut sub, &accepted.operation_id).await;
    assert!(matches!(outcome, OperationOutcome::Succeeded { .. }));

    // Ledger leg 1: the event carries the same text (same source).
    let mut event_text = None;
    for event in events {
        if let philo_agent_runtime::AgentEvent::ToolExecutionCompleted { result, .. } = event {
            event_text = result.content().map(str::to_owned);
        }
    }
    assert_eq!(event_text.as_deref(), Some(expected_text.as_str()));

    // Ledger leg 2: the durable session fact.
    let view = sessions
        .context_view(&philo_session::SessionId::new("m10-001"))
        .await
        .expect("view");
    let durable = view
        .messages()
        .iter()
        .find_map(|message| match message {
            ContextMessage::ToolResult { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .expect("durable tool result");
    assert_eq!(
        durable,
        ToolResultOutcome::Success {
            content: expected_text.clone()
        }
    );

    // Ledger leg 3: the tool-result text in the next request body.
    let bodies = transport.request_bodies();
    assert_eq!(bodies.len(), 2);
    assert_eq!(
        bodies[1]["messages"][3]["content"][0]["text"],
        serde_json::Value::String(expected_text),
        "the model sees exactly the durable bytes"
    );
}
