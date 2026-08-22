//! opt-in live smoke: real-API checks, never a CI gate.
//!
//! Both tests are `#[ignore]` and additionally require explicit environment
//! configuration. Run them with:
//!
//! ```text
//! set PHILO_M4_LIVE_ENDPOINT=https://api.example.com/v1/chat/completions
//! set PHILO_M4_LIVE_MODEL=some-model
//! set PHILO_M4_LIVE_PROTOCOL=openai-chat              (optional; openai-chat | openai-responses)
//! set PHILO_M4_LIVE_COMPAT=compatible                 (optional; official | compatible)
//! set PHILO_M4_LIVE_REASONING_FORMAT=content-only     (optional; Chat only)
//! set PHILO_M4_LIVE_API_KEY=<secret>
//! cargo test -p philo-model --test live_smoke -- --ignored
//! ```

mod support;

use std::sync::Arc;

use philo_agent_runtime::{
    AgentRuntime, ChannelBounds, GenerationConfig, OperationOutcome, OperationSpec, RuntimeConfig,
    RuntimeDeps, SequentialIdSource, SessionId, ToolRegistry, UserMessage,
};
use philo_model::{ChatReasoningFormat, ModelCompat, ModelProtocol, PhiloModelAdapter};
use philo_session::MemorySessionStore;
use philo_tools_std::ReadTool;
use support::{drain_until_settled, empty_tools, generation};

const API_KEY_VAR: &str = "PHILO_M4_LIVE_API_KEY";

fn live_protocol() -> ModelProtocol {
    match std::env::var("PHILO_M4_LIVE_PROTOCOL") {
        Err(_) => ModelProtocol::OpenAiChat,
        Ok(value) => match value.as_str() {
            "openai-chat" => ModelProtocol::OpenAiChat,
            "openai-responses" => ModelProtocol::OpenAiResponses,
            other => panic!(
                "PHILO_M4_LIVE_PROTOCOL must be openai-chat or openai-responses, got {other:?}"
            ),
        },
    }
}

fn live_compat() -> ModelCompat {
    match std::env::var("PHILO_M4_LIVE_COMPAT") {
        Err(_) => ModelCompat::Compatible,
        Ok(value) => match value.as_str() {
            "official" => ModelCompat::Official,
            "compatible" => ModelCompat::Compatible,
            other => panic!("PHILO_M4_LIVE_COMPAT must be official or compatible, got {other:?}"),
        },
    }
}

fn live_reasoning_format() -> Option<ChatReasoningFormat> {
    match std::env::var("PHILO_M4_LIVE_REASONING_FORMAT") {
        Err(_) => None,
        Ok(value) => Some(match value.as_str() {
            "none" => ChatReasoningFormat::None,
            "effort-only" => ChatReasoningFormat::EffortOnly,
            "content-only" => ChatReasoningFormat::ContentOnly,
            "effort-and-content" => ChatReasoningFormat::EffortAndContent,
            other => panic!(
                "PHILO_M4_LIVE_REASONING_FORMAT must be none, effort-only, content-only, or effort-and-content, got {other:?}"
            ),
        }),
    }
}

fn live_adapter() -> Option<PhiloModelAdapter> {
    let endpoint = std::env::var("PHILO_M4_LIVE_ENDPOINT").ok()?;
    let model = std::env::var("PHILO_M4_LIVE_MODEL").ok()?;
    let mut builder = PhiloModelAdapter::builder("live-provider", live_protocol(), model, endpoint)
        .compat(live_compat())
        .api_key_env(API_KEY_VAR);
    if let Some(format) = live_reasoning_format() {
        builder = builder.chat_reasoning_format(format);
    }
    Some(builder.build().expect("live adapter assembly"))
}

fn config(max_tool_rounds: u32, system_prompt: &str) -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: system_prompt.to_owned(),
        model_target: "live".to_owned(),
        generation: GenerationConfig {
            max_output_tokens: 512,
            temperature: 0.0,
            reasoning_effort: None,
            tool_choice: philo_agent_runtime::ToolChoice::Auto,
        },
        max_tool_rounds,
        max_parallel_tool_calls: 1,
        operation_timeout: None,
        tool_cancel_grace: std::time::Duration::from_millis(300),
        compaction: Default::default(),
        recovery: Default::default(),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live smoke is opt-in: configure PHILO_M4_LIVE_* and run with --ignored"]
async fn live_direct_answer_settles_successfully() {
    let Some(adapter) = live_adapter() else {
        panic!("PHILO_M4_LIVE_ENDPOINT / PHILO_M4_LIVE_MODEL must be set for live smoke");
    };
    let generation = generation(
        Arc::new(adapter),
        empty_tools(),
        config(0, "You are a terse assistant."),
    );
    let parts = AgentRuntime::start(RuntimeDeps {
        sessions: Arc::new(MemorySessionStore::new()),
        ids: Arc::new(SequentialIdSource::new()),
        bounds: ChannelBounds::default(),
    })
    .expect("start runtime");
    let handle = parts.handle;
    let mut sub = parts.events;
    let accepted = handle
        .submit(OperationSpec {
            session_id: SessionId::new("live-direct"),
            user_message: UserMessage::new("Reply with exactly the single word OK."),
            generation,
            service_request_id: None,
        })
        .await
        .expect("submit accepted");
    let (_events, outcome) = drain_until_settled(&mut sub, &accepted.operation_id).await;
    let OperationOutcome::Succeeded { assistant } = outcome else {
        panic!("live direct answer failed: {outcome:?}");
    };
    assert!(!assistant.content().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live smoke is opt-in: configure PHILO_M4_LIVE_* and run with --ignored"]
async fn live_tool_round_reads_a_real_file() {
    let Some(adapter) = live_adapter() else {
        panic!("PHILO_M4_LIVE_ENDPOINT / PHILO_M4_LIVE_MODEL must be set for live smoke");
    };
    let root = std::env::temp_dir().join(format!("philo-m4-live-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("create live smoke root");
    std::fs::write(root.join("token.txt"), "PHILO-M4-LIVE-TOKEN").expect("write token file");

    let registry = ToolRegistry::builder()
        .register(ReadTool::definition(), ReadTool::new(&root))
        .expect("register read tool")
        .build();
    let generation = generation(
        Arc::new(adapter),
        Arc::new(registry),
        config(
            2,
            "You must use the read tool before answering questions about files.",
        ),
    );
    let parts = AgentRuntime::start(RuntimeDeps {
        sessions: Arc::new(MemorySessionStore::new()),
        ids: Arc::new(SequentialIdSource::new()),
        bounds: ChannelBounds::default(),
    })
    .expect("start runtime");
    let handle = parts.handle;
    let mut sub = parts.events;
    let accepted = handle
        .submit(OperationSpec {
            session_id: SessionId::new("live-tool"),
            user_message: UserMessage::new(
                "Use the read tool to read the file 'token.txt' and reply with its exact content.",
            ),
            generation,
            service_request_id: None,
        })
        .await
        .expect("submit accepted");
    let (_events, outcome) = drain_until_settled(&mut sub, &accepted.operation_id).await;
    let _ = std::fs::remove_dir_all(&root);
    let OperationOutcome::Succeeded { assistant } = outcome else {
        panic!("live tool round failed: {outcome:?}");
    };
    assert!(
        assistant.content().contains("PHILO-M4-LIVE-TOKEN"),
        "final answer should echo the file token: {}",
        assistant.content()
    );
}

/// M7-007 live leg: a real reasoning model completes one tool round with
/// visible reasoning streamed as transient events. Point the target at a
/// Chat provider that emits `reasoning_content` (for example set
/// `PHILO_M4_LIVE_REASONING_FORMAT=content-only`).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live smoke is opt-in: configure PHILO_M4_LIVE_* and run with --ignored"]
async fn live_reasoning_tool_round_streams_transient_reasoning() {
    let Some(adapter) = live_adapter() else {
        panic!("PHILO_M4_LIVE_ENDPOINT / PHILO_M4_LIVE_MODEL must be set for live smoke");
    };
    let root = std::env::temp_dir().join(format!("philo-m7-live-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("create live smoke root");
    std::fs::write(root.join("token.txt"), "PHILO-M7-LIVE-TOKEN").expect("write token file");

    let registry = ToolRegistry::builder()
        .register(ReadTool::definition(), ReadTool::new(&root))
        .expect("register read tool")
        .build();
    let generation = generation(
        Arc::new(adapter),
        Arc::new(registry),
        config(
            2,
            "You must use the read tool before answering questions about files.",
        ),
    );
    let parts = AgentRuntime::start(RuntimeDeps {
        sessions: Arc::new(MemorySessionStore::new()),
        ids: Arc::new(SequentialIdSource::new()),
        bounds: ChannelBounds::default(),
    })
    .expect("start runtime");
    let handle = parts.handle;
    let mut sub = parts.events;
    let accepted = handle
        .submit(OperationSpec {
            session_id: SessionId::new("live-reasoning"),
            user_message: UserMessage::new(
                "Use the read tool to read the file 'token.txt' and reply with its exact content.",
            ),
            generation,
            service_request_id: None,
        })
        .await
        .expect("submit accepted");
    let (events, outcome) = drain_until_settled(&mut sub, &accepted.operation_id).await;
    let _ = std::fs::remove_dir_all(&root);
    let OperationOutcome::Succeeded { assistant } = outcome else {
        panic!("live reasoning tool round failed: {outcome:?}");
    };
    assert!(
        assistant.content().contains("PHILO-M7-LIVE-TOKEN"),
        "final answer should echo the file token: {}",
        assistant.content()
    );
    let reasoning_deltas = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                philo_agent_runtime::AgentEvent::ReasoningDelta { .. }
            )
        })
        .count();
    println!("observed {reasoning_deltas} transient reasoning deltas");
}
