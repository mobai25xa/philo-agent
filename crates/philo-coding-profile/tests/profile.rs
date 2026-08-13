//! PROFILE-001: the coding profile owns exactly the scenario knowledge —
//! tool lineup, system prompt, and runtime defaults — and stays overridable.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use philo_coding_profile::{CodingProfile, DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_MAX_TOOL_ROUNDS};
use philo_tools::{ToolInvocation, ToolPort};

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
    }
}

#[test]
fn registry_contains_the_six_coding_tools_with_fixed_effect_classes() {
    use philo_tools::EffectClass;
    let profile = CodingProfile::new(std::env::temp_dir());
    let definitions = profile.tool_registry().definitions();
    let lineup: Vec<(&str, EffectClass)> = definitions
        .iter()
        .map(|d| (d.name(), d.effect_class()))
        .collect();
    assert_eq!(
        lineup,
        [
            ("read", EffectClass::ReadOnly),
            ("list", EffectClass::ReadOnly),
            ("grep", EffectClass::ReadOnly),
            ("write", EffectClass::Workspace),
            ("edit", EffectClass::Workspace),
            ("shell", EffectClass::System),
        ],
        "the M10 coding lineup with its factual classification"
    );
}

#[test]
fn read_tool_is_wired_to_the_workspace_root() {
    let root = std::env::temp_dir().join(format!("philo-profile-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("create root");
    std::fs::write(root.join("probe.txt"), "profile probe content").expect("write probe");

    let registry = CodingProfile::new(&root).tool_registry();
    let result = block_on(registry.invoke(ToolInvocation::new(
        "call-1",
        "read",
        r#"{"path":"probe.txt"}"#,
    )))
    .expect("invocation is not an infrastructure failure");
    let content = result
        .result()
        .content()
        .expect("read succeeds against the root");
    assert!(content.contains("profile probe content"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn system_prompt_is_non_empty_and_mentions_the_tool_surface() {
    let prompt = CodingProfile::system_prompt();
    assert!(!prompt.trim().is_empty());
    for tool in ["read", "list", "grep", "write", "edit", "shell"] {
        assert!(
            prompt.contains(tool),
            "the prompt describes the `{tool}` tool"
        );
    }
}

#[test]
fn defaults_are_available_and_reasonable() {
    let generation = CodingProfile::generation_defaults();
    assert_eq!(generation.max_output_tokens, DEFAULT_MAX_OUTPUT_TOKENS);
    assert!(generation.max_output_tokens >= 1024);
    assert_eq!(generation.temperature, 0.0);
    assert_eq!(generation.reasoning_effort, None);
    assert_eq!(
        generation.tool_choice,
        philo_agent_runtime::ToolChoice::Auto
    );

    let config = CodingProfile::new(".").runtime_config("provider/model");
    assert_eq!(config.model_target, "provider/model");
    assert_eq!(config.system_prompt, CodingProfile::system_prompt());
    assert_eq!(config.max_tool_rounds, DEFAULT_MAX_TOOL_ROUNDS);
}

#[test]
fn assembled_config_is_overridable_field_by_field() {
    let mut config = CodingProfile::new(".").runtime_config("provider/model");
    config.max_tool_rounds = 2;
    config.generation.max_output_tokens = 256;
    config.generation.reasoning_effort = Some(philo_agent_runtime::ReasoningEffort::High);
    config.system_prompt = "override".to_owned();

    assert_eq!(config.max_tool_rounds, 2);
    assert_eq!(config.generation.max_output_tokens, 256);
    assert_eq!(
        config.generation.reasoning_effort,
        Some(philo_agent_runtime::ReasoningEffort::High)
    );
    assert_eq!(config.system_prompt, "override");
}
