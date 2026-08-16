//! INTEGRATION-007: reasoning and observability end to end through
//! production public APIs over the OpenAI-compatible baseline.
//!
//! AgentRuntime + philo-model (Chat + Compatible + ContentOnly, stub transport) +
//! philo-tools-std + a real ToolRegistry + MemorySessionStore.

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use philo_agent_runtime::{
    AgentEvent, AgentFailureKind, AgentRuntime, GenerationConfig, ModelCallId, OperationHandle,
    OperationOutcome, ReasoningEffort, RuntimeConfig, SequentialIdSource, SessionId,
    SettlementDurability, ToolRegistry, UserMessage,
};
use philo_model::{ChatReasoningFormat, ModelCompat, ModelProtocol, PhiloModelAdapter};
use philo_session::{ContextMessage, MemorySessionStore, SessionStore};
use philo_tools_std::ReadTool;
use support::{StubResponse, StubTransport, sse, text_sse};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-m7-e2e-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn file(&self, relative: &str, content: &str) {
        std::fs::write(self.path.join(relative), content).expect("write fixture file");
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn runtime_over(
    transport: StubTransport,
    sessions: Arc<MemorySessionStore>,
    root: &TempRoot,
    reasoning_effort: Option<ReasoningEffort>,
    chat_reasoning_format: ChatReasoningFormat,
) -> AgentRuntime {
    let adapter = PhiloModelAdapter::builder(
        "stub-provider",
        ModelProtocol::OpenAiChat,
        "stub-model",
        support::STUB_ENDPOINT,
    )
    .compat(ModelCompat::Compatible)
    .chat_reasoning_format(chat_reasoning_format)
    .build_with_transport(transport)
    .expect("adapter assembly");
    let registry = ToolRegistry::builder()
        .register(ReadTool::definition(), ReadTool::new(&root.path))
        .expect("register read tool")
        .build();
    AgentRuntime::with_tools(
        Arc::new(adapter),
        sessions,
        Arc::new(SequentialIdSource::new()),
        RuntimeConfig {
            system_prompt: "You are the M7 integration assistant.".to_owned(),
            model_target: "stub-model".to_owned(),
            generation: GenerationConfig {
                max_output_tokens: 256,
                temperature: 0.0,
                reasoning_effort,
                tool_choice: philo_agent_runtime::ToolChoice::Auto,
            },
            max_tool_rounds: 2,
            operation_timeout: None,
            compaction: Default::default(),
        },
        Arc::new(registry),
    )
}

/// Round one: visible reasoning, then a `read` tool call, then usage.
fn reasoning_tool_round_sse(response_id: &str, call_id: &str, path: &str) -> Vec<u8> {
    let head = format!(
        r#"{{"id":"{response_id}","object":"chat.completion.chunk","model":"stub-r1","choices":[{{"index":0,"delta":{{"role":"assistant","reasoning_content":"planning "}},"finish_reason":null}}]}}"#
    );
    let more = r#"{"choices":[{"index":0,"delta":{"reasoning_content":"the read"},"finish_reason":null}]}"#
        .to_owned();
    let tool_call = format!(
        r#"{{"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"{call_id}","type":"function","function":{{"name":"read","arguments":"{{\"path\":\"{path}\"}}"}}}}]}},"finish_reason":null}}]}}"#
    );
    let finish = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":6,"completion_tokens_details":{"reasoning_tokens":4}}}"#
        .to_owned();
    sse(&[&head, &more, &tool_call, &finish, "[DONE]"])
}

/// Final round: visible reasoning, the answer text, then usage.
fn reasoning_answer_sse(response_id: &str, deltas: &[&str]) -> Vec<u8> {
    let head = format!(
        r#"{{"id":"{response_id}","object":"chat.completion.chunk","model":"stub-r1","choices":[{{"index":0,"delta":{{"role":"assistant","reasoning_content":"summarizing"}},"finish_reason":null}}]}}"#
    );
    let mut records = vec![head];
    for delta in deltas {
        records.push(format!(
            r#"{{"choices":[{{"index":0,"delta":{{"content":{}}},"finish_reason":null}}]}}"#,
            serde_json::Value::String((*delta).to_owned())
        ));
    }
    records.push(
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":24,"completion_tokens":8}}"#
            .to_owned(),
    );
    records.push("[DONE]".to_owned());
    let records: Vec<&str> = records.iter().map(String::as_str).collect();
    sse(&records)
}

async fn drain(handle: &mut OperationHandle) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = handle.next_event().await {
        events.push(event);
    }
    events
}

/// M7-001/002/005/006 end to end: a multi-round loop streams transient
/// reasoning and usage events per call, replays the reasoning state into the
/// same turn's next request, keeps everything out of the Session, and starts
/// the next turn without any stale reasoning.
#[tokio::test]
async fn m7_e2e_reasoning_flows_replay_and_stay_out_of_the_session() {
    let root = TempRoot::new();
    root.file("hello.txt", "M7 content");
    let transport = StubTransport::new([
        StubResponse::Sse(reasoning_tool_round_sse("resp-1", "call-1", "hello.txt")),
        StubResponse::Sse(reasoning_answer_sse("resp-2", &["The file says M7."])),
        // Turn two: a plain baseline stream without reasoning or usage.
        StubResponse::Sse(text_sse("resp-3", "stub-r1", &["No new events here."])),
    ]);
    let sessions = Arc::new(MemorySessionStore::new());
    let agent = runtime_over(
        transport.clone(),
        sessions.clone(),
        &root,
        None,
        ChatReasoningFormat::ContentOnly,
    );

    let mut handle = agent
        .prompt(SessionId::new("m7"), UserMessage::new("read hello.txt"))
        .await
        .expect("prompt accepted");
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "The file says M7."
    ));
    let events = drain(&mut handle).await;

    // Each call's reasoning surfaces between its ModelCallStarted and its
    // assembled completion, tagged with that call's id.
    let reasoning: Vec<(&str, &str)> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ReasoningDelta {
                model_call_id,
                text,
            } => Some((model_call_id.as_str(), text.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        reasoning,
        vec![
            ("turn-1:model-call:1", "planning "),
            ("turn-1:model-call:1", "the read"),
            ("turn-1:model-call:2", "summarizing"),
        ]
    );
    let batch_requested = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ToolBatchRequested { .. }))
        .expect("tool batch requested");
    let first_reasoning = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ReasoningDelta { .. }))
        .unwrap();
    assert!(first_reasoning < batch_requested);

    // Usage observations arrive per call; the last one wins downstream.
    let usage_calls: Vec<(&str, Option<u64>)> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ModelUsageUpdated {
                model_call_id,
                usage,
            } => Some((model_call_id.as_str(), usage.reasoning_tokens)),
            _ => None,
        })
        .collect();
    assert_eq!(
        usage_calls,
        vec![
            ("turn-1:model-call:1", Some(4)),
            ("turn-1:model-call:2", None),
        ]
    );

    // The same turn's second request replays the first call's reasoning.
    let bodies = transport.request_bodies();
    let second_assistant = &bodies[1]["messages"][2];
    assert_eq!(second_assistant["role"], "assistant");
    assert_eq!(second_assistant["reasoning_content"], "planning the read");
    assert_eq!(second_assistant["tool_calls"][0]["id"], "call-1");

    // Durable facts stay reasoning-free: exactly the 0.4-contract entries.
    let view = sessions
        .context_view(&philo_session::SessionId::new("m7"))
        .await
        .expect("view");
    assert_eq!(view.revision(), philo_session::SessionRevision::new(4));
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
    assert_eq!(kinds, vec!["user", "calls", "result", "assistant"]);
    assert!(view.messages().iter().all(|message| match message {
        ContextMessage::Assistant { content } => !content.contains("planning"),
        _ => true,
    }));

    // Turn two: no stale reasoning in the request, and a stream without new
    // events keeps the baseline event vocabulary (wildcard-armed consumers
    // in this external crate compile against #[non_exhaustive]).
    let mut second = agent
        .prompt(SessionId::new("m7"), UserMessage::new("continue"))
        .await
        .expect("second prompt accepted");
    assert!(matches!(
        second.wait().await,
        OperationOutcome::Succeeded { .. }
    ));
    let second_events = drain(&mut second).await;
    assert!(!second_events.iter().any(|event| matches!(
        event,
        AgentEvent::ReasoningDelta { .. } | AgentEvent::ModelUsageUpdated { .. }
    )));
    let bodies = transport.request_bodies();
    let turn_two_request = &bodies[2];
    let assistants: Vec<&serde_json::Value> = turn_two_request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "assistant")
        .collect();
    assert!(
        assistants
            .iter()
            .all(|message| message.get("reasoning_content").is_none()),
        "no reasoning crosses the turn boundary"
    );

    // Sanity: the wildcard consumption stance in this external crate.
    let mut saw_reasoning = false;
    for event in &events {
        match event {
            AgentEvent::ReasoningDelta { .. } => saw_reasoning = true,
            AgentEvent::ModelUsageUpdated { model_call_id, .. } => {
                assert_eq!(ModelCallId::new(model_call_id.as_str()), *model_call_id);
            }
            _ => {}
        }
    }
    assert!(saw_reasoning);
}

/// M7-004 end to end: an effort the target cannot honor fails capability
/// negotiation before any transport call and settles the operation Failed
/// through the established model-error path.
#[tokio::test]
async fn m7_e2e_unsupported_effort_settles_the_operation_failed() {
    let root = TempRoot::new();
    let transport = StubTransport::new([StubResponse::Sse(text_sse("resp-1", "stub", &["never"]))]);
    let sessions = Arc::new(MemorySessionStore::new());
    let agent = runtime_over(
        transport.clone(),
        sessions.clone(),
        &root,
        Some(ReasoningEffort::High),
        ChatReasoningFormat::None,
    );

    let handle = agent
        .prompt(SessionId::new("m7-effort"), UserMessage::new("hi"))
        .await
        .expect("prompt accepted");
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Failed { failure, durability }
            if failure.kind() == AgentFailureKind::ModelCall
                && failure.message().contains("Capability")
                && durability == SettlementDurability::Confirmed
    ));
    assert!(
        transport.requests().is_empty(),
        "negotiation rejects before any transport call"
    );
    let view = sessions
        .context_view(&philo_session::SessionId::new("m7-effort"))
        .await
        .expect("view");
    assert_eq!(
        view.revision(),
        philo_session::SessionRevision::new(2),
        "turn start and failure settlement are the only durable facts"
    );
}
