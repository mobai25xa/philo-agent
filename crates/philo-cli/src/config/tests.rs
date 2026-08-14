use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use clap::Parser;
use philo_agent_runtime::ReasoningEffort;
use philo_model::ModelProtocol;

use super::file::{Layer, Sourced, load_layers};
use super::resolve::{
    Verbosity, parse_protocol, parse_reasoning_effort, parse_verbosity, validate_reasoning_effort,
};
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
fn deployment_headers_merge_case_insensitively_and_keep_per_header_sources() {
    let dir = TempDir::new();
    let global = dir.write(
        "headers-global.toml",
        "[deployment]\nendpoint = \"https://example.test\"\n\
         [deployment.headers]\n\"User-Agent\" = \"global-agent\"\n\
         \"X-Route\" = \"global-route\"\n",
    );
    let project = dir.write(
        "headers-project.toml",
        "[deployment.headers]\n\"user-agent\" = \"project-agent\"\n\
         \"X-Project\" = \"project-value\"\n",
    );

    let config = load_layers(Some(&global), Some(&project)).expect("loads");
    assert_eq!(config.headers.len(), 3);
    assert_eq!(config.headers["user-agent"].layer, Layer::Project);
    assert_eq!(config.headers["x-route"].layer, Layer::Global);
    assert_eq!(config.headers["x-project"].layer, Layer::Project);

    let settings = super::resolve::resolve(&resolvable_cli(), &config).expect("resolves");
    assert_eq!(
        settings
            .deployment
            .request_headers
            .names()
            .collect::<Vec<_>>(),
        ["user-agent", "x-project", "x-route"]
    );
    assert!(settings.entries.iter().any(|entry| {
        entry.key == "header.user-agent"
            && entry.value == "project-agent"
            && entry.source == "project"
    }));
    for key in ["header.x-project", "header.x-route"] {
        assert!(
            settings
                .entries
                .iter()
                .any(|entry| { entry.key == key && entry.value == "<configured>" })
        );
    }
    let debug = format!("{:?}", settings.deployment);
    assert!(!debug.contains("global-route"));
    assert!(!debug.contains("project-value"));
}

#[test]
fn default_user_agent_is_reported_when_no_override_exists() {
    let settings =
        super::resolve::resolve(&resolvable_cli(), &deployment_file()).expect("resolves");
    assert!(settings.entries.iter().any(|entry| {
        entry.key == "header.user-agent"
            && entry.value == philo_model::DEFAULT_USER_AGENT
            && entry.source == "default"
    }));
}

#[test]
fn reserved_header_errors_name_the_layer_without_exposing_the_value() {
    let dir = TempDir::new();
    let global = dir.write(
        "headers-reserved.toml",
        "[deployment]\nendpoint = \"https://example.test\"\n\
         [deployment.headers]\nAuthorization = \"Bearer do-not-print\"\n",
    );
    let config = load_layers(Some(&global), None).expect("syntax and types load");

    let error = super::resolve::resolve(&resolvable_cli(), &config)
        .err()
        .expect("credential header must fail");
    assert!(error.0.contains("authorization"), "{error:?}");
    assert!(error.0.contains("global config"), "{error:?}");
    assert!(!error.0.contains("do-not-print"), "{error:?}");
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
         [compaction]\n\
         context_budget = 96000\n\
         auto_threshold = 0.75\n\
         keep_recent_turns = 6\n\
         estimate_bytes_per_token = 4\n\
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
    assert_eq!(
        config
            .compaction_context_budget
            .expect("compaction budget")
            .value,
        96_000
    );
    assert_eq!(
        config
            .compaction_auto_threshold
            .expect("automatic threshold")
            .value,
        0.75
    );
    assert_eq!(
        config
            .compaction_keep_recent_turns
            .expect("recent turns")
            .value,
        6
    );
    assert_eq!(
        config
            .compaction_estimate_bytes_per_token
            .expect("estimation coefficient")
            .value,
        4
    );
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
    assert_eq!(
        parse_protocol("openai-chat-compatible-reasoning-effort").unwrap(),
        ModelProtocol::OpenAiChatCompatibleReasoningEffort
    );
    assert!(parse_protocol("grpc").is_err());
    assert_eq!(parse_verbosity("quiet").unwrap(), Verbosity::Quiet);
    assert!(parse_verbosity("loud").is_err());
}

#[test]
fn reasoning_effort_is_validated_against_the_selected_protocol() {
    assert!(
        validate_reasoning_effort(ModelProtocol::OpenAiResponses, ReasoningEffort::High).is_ok()
    );
    assert!(
        validate_reasoning_effort(ModelProtocol::OpenAiChat, ReasoningEffort::VeryHigh).is_ok()
    );
    assert!(
        validate_reasoning_effort(
            ModelProtocol::OpenAiChatCompatibleReasoningEffort,
            ReasoningEffort::Minimal,
        )
        .is_ok()
    );
    assert!(
        validate_reasoning_effort(ModelProtocol::AnthropicMessages, ReasoningEffort::Maximum)
            .is_ok()
    );

    for protocol in [
        ModelProtocol::OpenAiChatCompatible,
        ModelProtocol::OpenAiChatReasoningContent,
    ] {
        let error = validate_reasoning_effort(protocol, ReasoningEffort::High)
            .expect_err("compatible chat profiles do not accept reasoning effort");
        assert!(error.0.contains("unsupported by protocol"));
    }
    assert!(
        validate_reasoning_effort(ModelProtocol::AnthropicMessages, ReasoningEffort::Minimal)
            .is_err()
    );
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
        protocol: Some(Sourced {
            value: "openai-chat".to_owned(),
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

fn resolvable_cli() -> Cli {
    Cli::try_parse_from(["philo", "--model", "m", "hello"]).expect("valid CLI")
}

fn deployment_file() -> super::file::FileConfig {
    super::file::FileConfig {
        endpoint: Some(Sourced {
            value: "https://example.test".to_owned(),
            layer: Layer::Global,
        }),
        ..super::file::FileConfig::default()
    }
}

#[test]
fn explicit_compaction_values_override_the_deployment_hint() {
    let mut file = deployment_file();
    file.context_window = Some(Sourced {
        value: 128_000,
        layer: Layer::Global,
    });
    file.compaction_context_budget = Some(Sourced {
        value: 96_000,
        layer: Layer::Project,
    });
    file.compaction_auto_threshold = Some(Sourced {
        value: 0.65,
        layer: Layer::Project,
    });
    file.compaction_keep_recent_turns = Some(Sourced {
        value: 7,
        layer: Layer::Project,
    });
    file.compaction_estimate_bytes_per_token = Some(Sourced {
        value: 5,
        layer: Layer::Project,
    });

    let settings = super::resolve::resolve(&resolvable_cli(), &file).expect("resolves");
    assert_eq!(settings.context_window, Some(128_000));
    assert_eq!(settings.compaction.context_budget, Some(96_000));
    assert_eq!(settings.compaction.auto_threshold, 0.65);
    assert_eq!(settings.compaction.keep_recent_turns, 7);
    assert_eq!(settings.compaction.estimate_bytes_per_token, 5);
    assert!(settings.entries.iter().any(|entry| {
        entry.key == "context_budget" && entry.value == "96000" && entry.source == "project"
    }));
}

#[test]
fn deployment_context_window_is_the_budget_fallback_and_policy_defaults_are_stable() {
    let mut file = deployment_file();
    file.context_window = Some(Sourced {
        value: 128_000,
        layer: Layer::Global,
    });

    let settings = super::resolve::resolve(&resolvable_cli(), &file).expect("resolves");
    assert_eq!(settings.compaction.context_budget, Some(128_000));
    assert_eq!(
        settings.compaction,
        philo_agent_runtime::CompactionConfig {
            context_budget: Some(128_000),
            ..philo_agent_runtime::CompactionConfig::default()
        }
    );
    assert!(settings.entries.iter().any(|entry| {
        entry.key == "context_budget" && entry.value == "128000" && entry.source == "global"
    }));
    for key in [
        "auto_threshold",
        "keep_recent_turns",
        "estimate_bytes_per_token",
    ] {
        assert!(
            settings
                .entries
                .iter()
                .any(|entry| entry.key == key && entry.source == "default"),
            "{key} reports its default source"
        );
    }

    let settings =
        super::resolve::resolve(&resolvable_cli(), &deployment_file()).expect("resolves");
    assert_eq!(settings.compaction.context_budget, None);
    assert!(settings.entries.iter().any(|entry| {
        entry.key == "context_budget" && entry.value == "none" && entry.source == "default"
    }));
}

#[test]
fn compaction_threshold_must_be_a_finite_ratio() {
    for value in [0.0, -0.1, 1.01, f64::NAN, f64::INFINITY] {
        let mut file = deployment_file();
        file.compaction_auto_threshold = Some(Sourced {
            value,
            layer: Layer::Project,
        });
        let error = super::resolve::resolve(&resolvable_cli(), &file)
            .err()
            .expect("invalid threshold must fail");
        assert!(
            error.0.contains("greater than 0 and at most 1"),
            "{error:?}"
        );
    }
}

#[test]
fn compaction_layers_merge_key_by_key() {
    let dir = TempDir::new();
    let global = dir.write(
        "compact-global.toml",
        "[compaction]\ncontext_budget = 64000\nauto_threshold = 0.7\nkeep_recent_turns = 8\n",
    );
    let project = dir.write(
        "compact-project.toml",
        "[compaction]\ncontext_budget = 96000\nestimate_bytes_per_token = 4\n",
    );

    let config = load_layers(Some(&global), Some(&project)).expect("loads");
    assert!(config.warnings.is_empty());
    assert_eq!(
        config.compaction_context_budget.expect("budget"),
        Sourced {
            value: 96_000,
            layer: Layer::Project,
        }
    );
    assert_eq!(
        config.compaction_auto_threshold.expect("threshold"),
        Sourced {
            value: 0.7,
            layer: Layer::Global,
        }
    );
    assert_eq!(
        config.compaction_keep_recent_turns.expect("turns"),
        Sourced {
            value: 8,
            layer: Layer::Global,
        }
    );
    assert_eq!(
        config
            .compaction_estimate_bytes_per_token
            .expect("coefficient"),
        Sourced {
            value: 4,
            layer: Layer::Project,
        }
    );
}
