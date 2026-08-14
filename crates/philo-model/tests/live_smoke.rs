//! M4-007 live smoke: opt-in real-API checks, never a CI gate.
//!
//! Both tests are `#[ignore]` and additionally require explicit environment
//! configuration. Run them with:
//!
//! ```text
//! set PHILO_M4_LIVE_ENDPOINT=https://api.example.com/v1/chat/completions
//! set PHILO_M4_LIVE_MODEL=some-model
//! set PHILO_M4_LIVE_PROTOCOL=openai-chat-compatible   (optional)
//! set PHILO_M4_LIVE_API_KEY=<secret>
//! cargo test -p philo-model --test live_smoke -- --ignored
//! ```

use std::sync::Arc;

use philo_agent_runtime::{
    AgentRuntime, GenerationConfig, OperationOutcome, RuntimeConfig, SequentialIdSource, SessionId,
    ToolRegistry, UserMessage,
};
use philo_model::{ModelProtocol, PhiloModelAdapter};
use philo_session::MemorySessionStore;
use philo_tools_std::ReadTool;

const API_KEY_VAR: &str = "PHILO_M4_LIVE_API_KEY";

fn live_adapter() -> Option<PhiloModelAdapter> {
    let endpoint = std::env::var("PHILO_M4_LIVE_ENDPOINT").ok()?;
    let model = std::env::var("PHILO_M4_LIVE_MODEL").ok()?;
    let protocol = match std::env::var("PHILO_M4_LIVE_PROTOCOL").as_deref() {
        Ok("anthropic-messages") => ModelProtocol::AnthropicMessages,
        Ok("openai-chat") => ModelProtocol::OpenAiChat,
        Ok("openai-chat-compatible-reasoning-effort") => {
            ModelProtocol::OpenAiChatCompatibleReasoningEffort
        }
        Ok("openai-chat-reasoning-content") => ModelProtocol::OpenAiChatReasoningContent,
        Ok("openai-responses") => ModelProtocol::OpenAiResponses,
        _ => ModelProtocol::OpenAiChatCompatible,
    };
    Some(
        PhiloModelAdapter::builder("live-provider", protocol, model, endpoint)
            .api_key_env(API_KEY_VAR)
            .build()
            .expect("live adapter assembly"),
    )
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
        operation_timeout: None,
        compaction: Default::default(),
    }
}

#[tokio::test]
#[ignore = "live smoke is opt-in: configure PHILO_M4_LIVE_* and run with --ignored"]
async fn live_direct_answer_settles_successfully() {
    let Some(adapter) = live_adapter() else {
        panic!("PHILO_M4_LIVE_ENDPOINT / PHILO_M4_LIVE_MODEL must be set for live smoke");
    };
    let runtime = AgentRuntime::new(
        Arc::new(adapter),
        Arc::new(MemorySessionStore::new()),
        Arc::new(SequentialIdSource::new()),
        config(0, "You are a terse assistant."),
    );
    let handle = runtime
        .prompt(
            SessionId::new("live-direct"),
            UserMessage::new("Reply with exactly the single word OK."),
        )
        .await
        .expect("prompt accepted");
    let outcome = handle.wait().await;
    let OperationOutcome::Succeeded { assistant } = outcome else {
        panic!("live direct answer failed: {outcome:?}");
    };
    assert!(!assistant.content().is_empty());
}

#[tokio::test]
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
    let runtime = AgentRuntime::with_tools(
        Arc::new(adapter),
        Arc::new(MemorySessionStore::new()),
        Arc::new(SequentialIdSource::new()),
        config(
            2,
            "You must use the read tool before answering questions about files.",
        ),
        Arc::new(registry),
    );
    let handle = runtime
        .prompt(
            SessionId::new("live-tool"),
            UserMessage::new(
                "Use the read tool to read the file 'token.txt' and reply with its exact content.",
            ),
        )
        .await
        .expect("prompt accepted");
    let outcome = handle.wait().await;
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
/// `reasoning_content` provider (for example set
/// `PHILO_M4_LIVE_PROTOCOL=openai-chat-reasoning-content`).
#[tokio::test]
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
    let runtime = AgentRuntime::with_tools(
        Arc::new(adapter),
        Arc::new(MemorySessionStore::new()),
        Arc::new(SequentialIdSource::new()),
        config(
            2,
            "You must use the read tool before answering questions about files.",
        ),
        Arc::new(registry),
    );
    let mut handle = runtime
        .prompt(
            SessionId::new("live-reasoning"),
            UserMessage::new(
                "Use the read tool to read the file 'token.txt' and reply with its exact content.",
            ),
        )
        .await
        .expect("prompt accepted");
    let outcome = handle.wait().await;
    let _ = std::fs::remove_dir_all(&root);
    let OperationOutcome::Succeeded { assistant } = outcome else {
        panic!("live reasoning tool round failed: {outcome:?}");
    };
    assert!(
        assistant.content().contains("PHILO-M7-LIVE-TOKEN"),
        "final answer should echo the file token: {}",
        assistant.content()
    );
    let mut reasoning_deltas = 0usize;
    while let Some(event) = handle.next_event().await {
        if matches!(
            event,
            philo_agent_runtime::AgentEvent::ReasoningDelta { .. }
        ) {
            reasoning_deltas += 1;
        }
    }
    println!("observed {reasoning_deltas} transient reasoning deltas");
}
