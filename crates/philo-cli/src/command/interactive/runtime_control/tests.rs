use std::path::Path;

use philo_agent_runtime::{
    AgentAvailability, CompactionConfig, CompactionReport, GenerationConfig, ModelCallSnapshot,
    ModelError, ModelEventStream, SequentialIdSource, ToolChoice, ToolRegistry,
};
use philo_model::{
    MemoryModelReplayStore, ModelCompat, ModelContinuationPolicy, ModelProtocol,
    ModelRequestHeaders,
};
use philo_tui::TuiScreen;

use crate::config::{ResolveFlags, Settings, Verbosity};

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

fn flags() -> ResolveFlags {
    ResolveFlags {
        model: None,
        data_dir: None,
        system: None,
        max_tool_rounds: None,
        reasoning_effort: None,
        verbose: false,
        quiet: false,
    }
}

fn settings(dir: &Path, model: &str) -> Settings {
    Settings {
        deployment: Deployment {
            provider: "test".to_owned(),
            protocol: ModelProtocol::OpenAiChat,
            model: model.to_owned(),
            endpoint: "https://example.test/v1/chat/completions".to_owned(),
            api_key_env: "PHILO_API_KEY".to_owned(),
            request_headers: ModelRequestHeaders::default(),
            compat: ModelCompat::Compatible,
            chat_reasoning_format: None,
            continuation_policy: ModelContinuationPolicy::StatelessLocalReplay,
        },
        data_dir: dir.to_path_buf(),
        context_window: None,
        compaction: CompactionConfig {
            context_budget: Some(64_000),
            auto_threshold: 0.7,
            keep_recent_turns: 6,
            estimate_bytes_per_token: 4,
        },
        reasoning_effort: None,
        max_tool_rounds: Some(1),
        max_parallel_tool_calls: None,
        operation_timeout: None,
        shell_timeout_secs: None,
        verbosity: Verbosity::Default,
        show_reasoning: true,
        screen: TuiScreen::Alternate,
        entries: vec![],
    }
}

fn control(dir: &Path) -> RuntimeControl {
    let settings = settings(dir, "model-a");
    RuntimeControl::new(
        settings.deployment.clone(),
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
            max_parallel_tool_calls: 1,
            operation_timeout: None,
            tool_cancel_grace: std::time::Duration::from_millis(300),
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
        flags(),
        &settings,
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

#[test]
fn ui_only_reload_does_not_rebuild_the_runtime() {
    let dir = temp_dir("ui-only");
    let control = control(&dir);
    let before = Arc::as_ptr(&control.runtime());
    let mut next = settings(&dir, "model-a");
    next.show_reasoning = false;
    next.entries = vec![crate::config::EffectiveSetting {
        key: "show_reasoning".to_owned(),
        value: "false".to_owned(),
        source: "project".to_owned(),
    }];

    let result = control.apply_settings(next).expect("ui apply");
    let ApplyResult::Applied {
        display,
        runtime_pending,
    } = result
    else {
        panic!("ui-only apply must succeed immediately");
    };
    assert!(!runtime_pending);
    assert!(!display.show_reasoning);
    assert_eq!(before, Arc::as_ptr(&control.runtime()));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn screen_only_reload_updates_config_entries_without_rebuilding() {
    let dir = temp_dir("screen-only");
    let control = control(&dir);
    let before = Arc::as_ptr(&control.runtime());
    let mut next = settings(&dir, "model-a");
    next.screen = TuiScreen::Inline;
    next.entries = vec![crate::config::EffectiveSetting {
        key: "screen".to_owned(),
        value: "inline".to_owned(),
        source: "project".to_owned(),
    }];

    let result = control.apply_settings(next).expect("screen apply");
    let ApplyResult::Applied {
        runtime_pending, ..
    } = result
    else {
        panic!("screen-only apply must succeed immediately");
    };
    assert!(!runtime_pending);
    assert_eq!(before, Arc::as_ptr(&control.runtime()));
    assert!(
        control
            .config_entries()
            .iter()
            .any(|entry| entry.key == "screen"
                && entry.value == "inline"
                && entry.source == "project"),
        "hot reload must show the configured token without switching the session screen"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn runtime_reload_queues_while_busy_and_applies_last_parse_on_idle() {
    let dir = temp_dir("queue-last");
    let control = control(&dir);
    let handle = control
        .prompt(SessionId::new("s"), UserMessage::new("hold"))
        .await
        .expect("admit");
    assert!(matches!(
        control.availability(),
        AgentAvailability::Busy { .. }
    ));

    let mut first = settings(&dir, "model-a");
    first.max_tool_rounds = Some(3);
    let ApplyResult::Applied {
        runtime_pending: true,
        ..
    } = control.apply_settings(first).expect("queue first")
    else {
        panic!("busy runtime apply must queue");
    };
    assert_eq!(
        control
            .assembly
            .lock()
            .expect("lock")
            .config
            .max_tool_rounds,
        1,
        "the serving snapshot stays on the old runtime"
    );

    let mut second = settings(&dir, "model-a");
    second.max_tool_rounds = Some(5);
    control.apply_settings(second).expect("replace pending");
    assert_eq!(
        control
            .assembly
            .lock()
            .expect("lock")
            .config
            .max_tool_rounds,
        1
    );

    handle.wait().await;
    assert!(matches!(control.availability(), AgentAvailability::Idle));
    let flushed = control
        .flush_pending()
        .expect("flush")
        .expect("pending apply");
    let ApplyResult::Applied {
        runtime_pending: false,
        ..
    } = flushed
    else {
        panic!("idle flush must apply");
    };
    assert_eq!(
        control
            .assembly
            .lock()
            .expect("lock")
            .config
            .max_tool_rounds,
        5,
        "only the last successful parse is applied"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn data_dir_reload_is_rejected_and_keeps_the_session_root() {
    let dir = temp_dir("data-dir");
    let control = control(&dir);
    let before = Arc::as_ptr(&control.runtime());
    let mut next = settings(&dir, "model-a");
    next.data_dir = dir.join("other");

    let error = control.apply_settings(next).expect_err("data_dir rejected");
    assert!(matches!(error, ApplyError::DataDir));
    assert_eq!(before, Arc::as_ptr(&control.runtime()));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn illegal_model_assembly_keeps_the_old_runtime() {
    let dir = temp_dir("bad-model");
    let control = control(&dir);
    let before = Arc::as_ptr(&control.runtime());
    let mut next = settings(&dir, "model-a");
    next.deployment.endpoint = "not-a-url".to_owned();

    let error = control.apply_settings(next).expect_err("assembly fails");
    assert!(matches!(error, ApplyError::Assembly(_)));
    assert_eq!(before, Arc::as_ptr(&control.runtime()));
    assert_eq!(
        control.assembly.lock().expect("lock").config.model_target,
        "model-a"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn idle_model_reload_rebuilds_the_runtime() {
    let dir = temp_dir("model-reload");
    let control = control(&dir);
    let before = Arc::as_ptr(&control.runtime());
    let next = settings(&dir, "model-b");

    let result = control.apply_settings(next).expect("idle model apply");
    let ApplyResult::Applied {
        display,
        runtime_pending: false,
    } = result
    else {
        panic!("idle model apply must swap");
    };
    assert_eq!(display.model_name, "model-b");
    assert_ne!(before, Arc::as_ptr(&control.runtime()));
    assert_eq!(
        control.assembly.lock().expect("lock").config.model_target,
        "model-b"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn model_rebuild_stays_idle_only_and_does_not_queue() {
    let dir = temp_dir("model-busy");
    let control = control(&dir);
    let handle = control
        .prompt(SessionId::new("s"), UserMessage::new("hold"))
        .await
        .expect("admit");

    let error = control
        .rebuild_model("model-b")
        .expect_err("busy /model fails immediately");
    assert!(error.message().contains("still running"));
    assert_eq!(
        control.assembly.lock().expect("lock").config.model_target,
        "model-a"
    );

    handle.wait().await;
    control.rebuild_model("model-b").expect("idle /model");
    assert_eq!(
        control.assembly.lock().expect("lock").config.model_target,
        "model-b"
    );
    let _ = std::fs::remove_dir_all(dir);
}
