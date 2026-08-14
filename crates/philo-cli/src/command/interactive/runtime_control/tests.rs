use philo_agent_runtime::{
    GenerationConfig, ModelCallSnapshot, ModelError, ModelEventStream, SequentialIdSource,
    ToolChoice, ToolRegistry,
};
use philo_model::ModelProtocol;

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
            protocol: ModelProtocol::OpenAiChatCompatible,
            model: "model-a".to_owned(),
            endpoint: "https://example.test/v1/chat/completions".to_owned(),
            api_key_env: "PHILO_API_KEY".to_owned(),
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
        },
        Arc::new(InertModel),
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
