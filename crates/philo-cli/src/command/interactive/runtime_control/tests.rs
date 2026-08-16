use philo_agent_runtime::{
    CompactionConfig, CompactionReport, GenerationConfig, ModelCallSnapshot, ModelError,
    ModelEventStream, SequentialIdSource, ToolChoice, ToolRegistry,
};
use philo_model::{
    MemoryModelReplayStore, ModelCompat, ModelContinuationPolicy, ModelProtocol,
    ModelRequestHeaders,
};

use super::*;

struct InertModel;

impl ModelPort for InertModel {
    fn start<'a>(
        &'a self,
        _request: ModelCallSnapshot,
    ) -> RuntimeFuture<'a, Result<Box<dyn ModelEventStream>, ModelError>> {
        Box::pin(async { Err(ModelError::new("not used in tests")) })
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "philo-cli-runtime-control-{}-{name}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("create dir");
    path
}

fn control(dir: &std::path::Path) -> RuntimeControl {
    RuntimeControl::new(
        Deployment {
            provider: "test".to_owned(),
            protocol: ModelProtocol::OpenAiChat,
            model: "model-a".to_owned(),
            endpoint: "https://example.test/v1/chat/completions".to_owned(),
            api_key_env: "PHILO_API_KEY".to_owned(),
            request_headers: ModelRequestHeaders::default(),
            compat: ModelCompat::Compatible,
            chat_reasoning_format: None,
            continuation_policy: ModelContinuationPolicy::StatelessLocalReplay,
        },
        RuntimeConfig {
            system_prompt: "s".to_owned(),
            model_target: "model-a".to_owned(),
            generation: GenerationConfig {
                max_output_tokens: 16,
                temperature: 0.0,
                reasoning_effort: None,
                tool_choice: ToolChoice::Auto,
            },
            max_tool_rounds: 1,
            operation_timeout: None,
            compaction: CompactionConfig {
                context_budget: Some(64_000),
                auto_threshold: 0.7,
                keep_recent_turns: 6,
                estimate_bytes_per_token: 4,
            },
        },
        Arc::new(InertModel),
        Arc::new(MemoryModelReplayStore::default()),
        Arc::new(JsonlSessionStore::open(dir).expect("open store")),
        Arc::new(SequentialIdSource::default()),
        Arc::new(ToolRegistry::empty()),
    )
}

#[test]
fn idle_model_rebuild_replaces_runtime_and_keeps_configuration() {
    let dir = temp_dir("rebuild");
    let control = control(&dir);
    let before = Arc::as_ptr(&control.runtime());

    control.rebuild_model("model-b").expect("idle rebuild");

    assert_ne!(before, Arc::as_ptr(&control.runtime()));
    assert_eq!(
        control.assembly.lock().expect("lock").config.model_target,
        "model-b"
    );
    assert_eq!(
        control.assembly.lock().expect("lock").config.compaction,
        CompactionConfig {
            context_budget: Some(64_000),
            auto_threshold: 0.7,
            keep_recent_turns: 6,
            estimate_bytes_per_token: 4,
        },
        "model rebuilding preserves the resolved compaction policy"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn reasoning_is_frozen_per_operation_without_rebuilding() {
    let dir = temp_dir("reasoning");
    let control = control(&dir);
    let before = Arc::as_ptr(&control.runtime());

    let first = control
        .prompt(SessionId::new("s"), UserMessage::new("first"))
        .await
        .expect("first admission");
    control.set_reasoning(ReasoningEffort::High);
    assert_eq!(before, Arc::as_ptr(&control.runtime()));
    let second = control
        .prompt(SessionId::new("s"), UserMessage::new("second"))
        .await
        .expect("queued admission");

    let state = control.reasoning.lock().expect("reasoning state");
    assert_eq!(state.operations.get(first.operation_id()), Some(&None));
    assert_eq!(
        state.operations.get(second.operation_id()),
        Some(&Some(ReasoningEffort::High))
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn compact_future_owns_the_runtime_and_does_not_borrow_the_control() {
    let dir = temp_dir("owned-compact");
    let control = control(&dir);

    let future = control.compact(SessionId::new("empty-session"));
    drop(control);

    assert_eq!(future.await, Ok(CompactionReport::NothingToCompact));
    let _ = std::fs::remove_dir_all(dir);
}
