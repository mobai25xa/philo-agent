use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use clap::Parser;
use philo_agent_runtime::ReasoningEffort;
use philo_model::ModelProtocol;

use super::file::{Layer, Sourced, load_layers};
use super::resolve::{Verbosity, parse_protocol, parse_reasoning_effort, parse_verbosity};
use crate::args::Cli;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-cli-config-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, text).expect("write config");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn absent_files_leave_everything_unset() {
    let config = load_layers(None, None).expect("no layers");
    assert!(config.model.is_none());
    assert!(config.warnings.is_empty());
}

#[test]
fn project_overrides_global_key_by_key() {
    let dir = TempDir::new();
    let global = dir.write(
        "global.toml",
        "[deployment]\nmodel = \"global-model\"\nendpoint = \"https://global.test\"\n",
    );
    let project = dir.write("project.toml", "[deployment]\nmodel = \"project-model\"\n");

    let config = load_layers(Some(&global), Some(&project)).expect("loads");
    assert_eq!(
        config.model,
        Some(Sourced {
            value: "project-model".to_owned(),
            layer: Layer::Project,
        })
    );
    assert_eq!(
        config.endpoint,
        Some(Sourced {
            value: "https://global.test".to_owned(),
            layer: Layer::Global,
        })
    );
}

#[test]
fn every_section_reads_its_key_domain() {
    let dir = TempDir::new();
    let path = dir.write(
        "all.toml",
        "[deployment]\n\
         model = \"m\"\n\
         endpoint = \"https://e.test\"\n\
         protocol = \"openai-chat\"\n\
         provider = \"p\"\n\
         api_key_env = \"MY_KEY_VAR\"\n\
         data_dir = \"/tmp/sessions\"\n\
         context_window = 128000\n\
         [defaults]\n\
         reasoning_effort = \"high\"\n\
         max_tool_rounds = 12\n\
         operation_timeout_secs = 300\n\
         [tools]\n\
         shell_timeout_secs = 90\n\
         [ui]\n\
         verbosity = \"verbose\"\n\
         show_reasoning = false\n",
    );

    let config = load_layers(Some(&path), None).expect("loads");
    assert_eq!(config.api_key_env.expect("api key env").value, "MY_KEY_VAR");
    assert_eq!(config.context_window.expect("window").value, 128_000);
    assert_eq!(config.max_tool_rounds.expect("rounds").value, 12);
    assert_eq!(config.shell_timeout_secs.expect("shell").value, 90);
    assert!(!config.show_reasoning.expect("reasoning").value);
}

#[test]
fn unknown_keys_warn_but_invalid_values_fail() {
    let dir = TempDir::new();
    let future = dir.write(
        "future.toml",
        "[deployment]\nmodel = \"m\"\nfuture = 1\n[telemetry]\nenabled = true\n",
    );
    let config = load_layers(Some(&future), None).expect("unknown keys are tolerated");
    assert_eq!(config.warnings.len(), 2);

    let wrong = dir.write("wrong.toml", "[deployment]\nmodel = 42\n");
    assert!(
        load_layers(Some(&wrong), None)
            .expect_err("wrong type")
            .0
            .contains("must be a string, found integer")
    );

    let zero = dir.write("zero.toml", "[defaults]\nmax_tool_rounds = 0\n");
    assert!(
        load_layers(Some(&zero), None)
            .expect_err("non-positive")
            .0
            .contains("must be a positive integer")
    );
}

#[test]
fn secret_values_are_refused_without_echoing_them() {
    let dir = TempDir::new();
    let secret = dir.write(
        "secret.toml",
        "[deployment]\napi_key = \"sk-not-a-real-key\"\n",
    );
    let error = load_layers(Some(&secret), None).expect_err("secret key refused");
    assert!(error.0.contains("would store a secret"));
    assert!(!error.0.contains("sk-not-a-real-key"));

    let misplaced = dir.write(
        "misplaced.toml",
        "[deployment]\napi_key_env = \"sk-must-not-render\"\n",
    );
    let error = load_layers(Some(&misplaced), None).expect_err("not an env name");
    assert!(error.0.contains("environment variable name"));
    assert!(!error.0.contains("sk-must-not-render"));

    let malformed = dir.write(
        "malformed.toml",
        "[deployment]\napi_key_env = \"sk-must-not-leak\n",
    );
    let error = load_layers(Some(&malformed), None).expect_err("invalid TOML");
    assert!(error.0.contains("invalid TOML"));
    assert!(!error.0.contains("sk-must-not-leak"));
}

#[test]
fn value_parsers_cover_the_supported_vocabulary() {
    for (text, expected) in [
        ("minimal", ReasoningEffort::Minimal),
        ("low", ReasoningEffort::Low),
        ("medium", ReasoningEffort::Medium),
        ("high", ReasoningEffort::High),
        ("very-high", ReasoningEffort::VeryHigh),
        ("maximum", ReasoningEffort::Maximum),
    ] {
        assert_eq!(parse_reasoning_effort(text).unwrap(), expected);
    }
    assert!(parse_reasoning_effort("extreme").is_err());
    assert_eq!(
        parse_protocol("openai-chat-reasoning-content").unwrap(),
        ModelProtocol::OpenAiChatReasoningContent
    );
    assert!(parse_protocol("grpc").is_err());
    assert_eq!(parse_verbosity("quiet").unwrap(), Verbosity::Quiet);
    assert!(parse_verbosity("loud").is_err());
}

#[test]
fn resolved_entries_use_cli_vocabulary_not_tui_types() {
    let cli = Cli::try_parse_from([
        "philo",
        "--model",
        "m",
        "--reasoning-effort",
        "high",
        "hello",
    ])
    .expect("valid CLI");
    let file = super::file::FileConfig {
        endpoint: Some(Sourced {
            value: "https://example.test".to_owned(),
            layer: Layer::Global,
        }),
        ..super::file::FileConfig::default()
    };

    let settings = super::resolve::resolve(&cli, &file).expect("resolves");
    assert!(
        settings
            .entries
            .iter()
            .any(|entry| entry.key == "model" && entry.source == "flag")
    );
}
