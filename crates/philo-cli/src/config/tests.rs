use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use clap::Parser;
use philo_agent_runtime::ReasoningEffort;
use philo_model::{ChatReasoningFormat, ModelCompat, ModelContinuationPolicy, ModelProtocol};
use philo_tui::TuiScreen;

use super::file::{Layer, Sourced, load_layers};
use super::resolve::{
    Verbosity, map_ui_screen, parse_compat, parse_continuation_policy, parse_protocol,
    parse_reasoning_effort, parse_reasoning_format, parse_verbosity, validate_reasoning_effort,
};
use crate::args::Cli;

struct TempDir(PathBuf);

static WATCH_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        .expect_err("credential header must fail");
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
         continuation = \"prefer-previous-response-id\"\n\
         compat = \"official\"\n\
         reasoning_format = \"effort-only\"\n\
         [compaction]\n\
         context_budget = 96000\n\
         auto_threshold = 0.75\n\
         keep_recent_turns = 6\n\
         estimate_bytes_per_token = 4\n\
         [defaults]\n\
         reasoning_effort = \"high\"\n\
         max_tool_rounds = 12\n\
         max_parallel_tool_calls = 4\n\
         operation_timeout_secs = 300\n\
         [tools]\n\
         shell_timeout_secs = 90\n\
         [ui]\n\
         verbosity = \"verbose\"\n\
         show_reasoning = false\n\
         screen = \"inline\"\n",
    );

    let config = load_layers(Some(&path), None).expect("loads");
    assert_eq!(config.api_key_env.expect("api key env").value, "MY_KEY_VAR");
    assert_eq!(config.context_window.expect("window").value, 128_000);
    assert_eq!(
        config.continuation.expect("continuation").value,
        "prefer-previous-response-id"
    );
    assert_eq!(config.compat.expect("compat").value, "official");
    assert_eq!(
        config.reasoning_format.expect("reasoning format").value,
        "effort-only"
    );
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
    assert_eq!(config.max_parallel_tool_calls.expect("parallel").value, 4);
    assert_eq!(config.shell_timeout_secs.expect("shell").value, 90);
    assert!(!config.show_reasoning.expect("reasoning").value);
    assert_eq!(config.screen.expect("screen").value, "inline");
}

#[test]
fn recovery_section_parses_every_key() {
    let dir = TempDir::new();
    let path = dir.write(
        "recovery.toml",
        "[recovery]\n\
         enabled = false\n\
         max_retries = 5\n\
         backoff_base_ms = 250\n\
         backoff_max_ms = 4000\n\
         response_head_timeout_secs = 30\n\
         stream_idle_timeout_secs = 60\n",
    );

    let config = load_layers(Some(&path), None).expect("loads");
    assert!(!config.recovery_enabled.expect("enabled").value);
    assert_eq!(config.recovery_max_retries.expect("retries").value, 5);
    assert_eq!(
        config
            .recovery_backoff_base_ms
            .expect("backoff base")
            .value,
        250
    );
    assert_eq!(
        config.recovery_backoff_max_ms.expect("backoff cap").value,
        4_000
    );
    assert_eq!(
        config
            .recovery_response_head_timeout_secs
            .expect("head timeout")
            .value,
        30
    );
    assert_eq!(
        config
            .recovery_stream_idle_timeout_secs
            .expect("idle timeout")
            .value,
        60
    );
}

#[test]
fn recovery_rejects_negative_and_invalid_values() {
    let dir = TempDir::new();
    let negative = dir.write("negative.toml", "[recovery]\nmax_retries = -1\n");
    assert!(
        load_layers(Some(&negative), None)
            .expect_err("negative retries")
            .0
            .contains("must be a non-negative integer")
    );

    let zero_base = dir.write("zero-base.toml", "[recovery]\nbackoff_base_ms = 0\n");
    assert!(
        load_layers(Some(&zero_base), None)
            .expect_err("zero backoff base")
            .0
            .contains("must be a positive integer")
    );

    let wrong_type = dir.write("wrong-type.toml", "[recovery]\nenabled = \"yes\"\n");
    assert!(
        load_layers(Some(&wrong_type), None)
            .expect_err("wrong type")
            .0
            .contains("must be a boolean, found string")
    );
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

    let parallel_zero = dir.write(
        "parallel-zero.toml",
        "[defaults]\nmax_parallel_tool_calls = 0\n",
    );
    assert!(
        load_layers(Some(&parallel_zero), None)
            .expect_err("parallel cap must be at least 1")
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
        parse_protocol("openai-chat").unwrap(),
        ModelProtocol::OpenAiChat
    );
    assert_eq!(
        parse_protocol("openai-responses").unwrap(),
        ModelProtocol::OpenAiResponses
    );
    assert!(parse_protocol("grpc").is_err());
    assert_eq!(parse_compat("official").unwrap(), ModelCompat::Official);
    assert_eq!(parse_compat("compatible").unwrap(), ModelCompat::Compatible);
    assert!(parse_compat("detect").is_err());
    assert_eq!(
        parse_reasoning_format("content-only").unwrap(),
        ChatReasoningFormat::ContentOnly
    );
    assert!(parse_reasoning_format("both").is_err());
    assert_eq!(
        parse_continuation_policy("prefer-previous-response-id").unwrap(),
        ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback
    );
    assert!(parse_continuation_policy("automatic").is_err());
    assert_eq!(parse_verbosity("quiet").unwrap(), Verbosity::Quiet);
    assert!(parse_verbosity("loud").is_err());
}

#[test]
fn ui_screen_parses_auto_alternate_inline_and_defaults_to_auto() {
    let dir = TempDir::new();
    for token in ["auto", "alternate", "inline"] {
        let path = dir.write(
            &format!("{token}.toml"),
            &format!("[ui]\nscreen = \"{token}\"\n"),
        );
        let config = load_layers(Some(&path), None).expect("loads");
        assert_eq!(config.screen.expect("screen").value, token);
        assert!(
            config
                .warnings
                .iter()
                .all(|warning| !warning.contains("screen"))
        );
    }

    let omitted = load_layers(None, None).expect("no layers");
    assert!(omitted.screen.is_none());

    let settings =
        super::resolve::resolve(&resolvable_cli(), &deployment_file()).expect("defaults resolve");
    assert!(settings.entries.iter().any(|entry| {
        entry.key == "screen" && entry.value == "auto" && entry.source == "default"
    }));
    assert_eq!(
        settings.screen,
        map_ui_screen("auto", std::env::var_os("ZELLIJ").is_some()).unwrap()
    );

    for (token, expected) in [
        (
            "auto",
            map_ui_screen("auto", std::env::var_os("ZELLIJ").is_some()).unwrap(),
        ),
        ("alternate", TuiScreen::Alternate),
        ("inline", TuiScreen::Inline),
    ] {
        let mut file = deployment_file();
        file.screen = Some(Sourced {
            value: token.to_owned(),
            layer: Layer::Project,
        });
        let settings = super::resolve::resolve(&resolvable_cli(), &file).expect("resolves");
        assert_eq!(settings.screen, expected);
        assert!(settings.entries.iter().any(|entry| {
            entry.key == "screen" && entry.value == token && entry.source == "project"
        }));
    }
}

#[test]
fn map_ui_screen_uses_zellij_only_for_auto() {
    assert_eq!(map_ui_screen("auto", true).unwrap(), TuiScreen::Inline);
    assert_eq!(map_ui_screen("auto", false).unwrap(), TuiScreen::Alternate);
    assert_eq!(
        map_ui_screen("alternate", true).unwrap(),
        TuiScreen::Alternate
    );
    assert_eq!(map_ui_screen("inline", false).unwrap(), TuiScreen::Inline);
    assert!(map_ui_screen("fullscreen", false).is_err());
}

#[test]
fn invalid_ui_screen_is_a_hard_error() {
    let mut file = deployment_file();
    file.screen = Some(Sourced {
        value: "fullscreen".to_owned(),
        layer: Layer::Global,
    });
    let error = super::resolve::resolve(&resolvable_cli(), &file)
        .expect_err("invalid screen must fail");
    assert!(
        error.0.contains("invalid [ui].screen 'fullscreen'"),
        "{error:?}"
    );
    assert!(
        error.0.contains("[ui].screen in the global config"),
        "{error:?}"
    );
}

#[test]
fn terminal_bg_parses_hex_and_flows_into_settings() {
    use super::resolve::parse_hex_color;

    assert_eq!(parse_hex_color("#1A1B26").unwrap(), (0x1a, 0x1b, 0x26));
    assert_eq!(parse_hex_color("1a1b26").unwrap(), (26, 27, 38));
    assert!(parse_hex_color("#1b2").is_err());
    assert!(parse_hex_color("#1a1b2g").is_err());
    assert!(parse_hex_color("#1a1b26ff").is_err());
    assert!(parse_hex_color("not-a-color").is_err());

    let mut file = deployment_file();
    file.terminal_bg = Some(Sourced {
        value: "#201f2a".to_owned(),
        layer: Layer::Project,
    });
    let settings = super::resolve::resolve(&resolvable_cli(), &file).expect("resolves");
    assert_eq!(settings.terminal_bg, Some((0x20, 0x1f, 0x2a)));
    assert!(settings.entries.iter().any(|entry| {
        entry.key == "terminal_bg" && entry.value == "#201f2a" && entry.source == "project"
    }));

    let unset =
        super::resolve::resolve(&resolvable_cli(), &deployment_file()).expect("defaults resolve");
    assert_eq!(unset.terminal_bg, None);
}

#[test]
fn retired_protocol_names_fail_with_migration_text() {
    let compatible = parse_protocol("openai-chat-compatible").expect_err("old dialect");
    assert!(compatible.0.contains("protocol=openai-chat"));
    assert!(compatible.0.contains("compat=compatible"));
    assert!(compatible.0.contains("reasoning_format=none"));

    let effort =
        parse_protocol("openai-chat-compatible-reasoning-effort").expect_err("old effort dialect");
    assert!(effort.0.contains("protocol=openai-chat"));
    assert!(effort.0.contains("compat=compatible"));
    assert!(effort.0.contains("reasoning_format=effort-only"));

    let content = parse_protocol("openai-chat-reasoning-content").expect_err("old content dialect");
    assert!(content.0.contains("protocol=openai-chat"));
    assert!(content.0.contains("compat=compatible"));
    assert!(content.0.contains("reasoning_format=content-only"));

    let anthropic = parse_protocol("anthropic-messages").expect_err("anthropic is unsupported");
    assert!(anthropic.0.contains("not supported"));
    assert!(!anthropic.0.contains("openai-chat"));
    assert!(!anthropic.0.contains("openai-responses"));
}

#[test]
fn response_continuation_support_is_a_hard_error() {
    let dir = TempDir::new();
    let path = dir.write(
        "old-support.toml",
        "[deployment]\nmodel = \"m\"\nresponse_continuation_support = \"official-openai\"\n",
    );
    let error = load_layers(Some(&path), None).expect_err("old key is refused");
    assert!(error.0.contains("removed"), "{error:?}");
    assert!(error.0.contains("prefer-previous-response-id"), "{error:?}");
    assert!(!error.0.contains("unknown key"), "{error:?}");
}

#[test]
fn defaults_are_chat_compatible_and_stateless() {
    let settings =
        super::resolve::resolve(&resolvable_cli(), &deployment_file()).expect("defaults resolve");
    assert_eq!(settings.deployment.protocol, ModelProtocol::OpenAiChat);
    assert_eq!(settings.deployment.compat, ModelCompat::Compatible);
    assert_eq!(settings.deployment.chat_reasoning_format, None);
    assert_eq!(
        settings.deployment.continuation_policy,
        ModelContinuationPolicy::StatelessLocalReplay
    );
    for (key, value) in [
        ("protocol", "openai-chat"),
        ("compat", "compatible"),
        ("continuation", "stateless-local-replay"),
    ] {
        assert!(
            settings.entries.iter().any(|entry| {
                entry.key == key && entry.value == value && entry.source == "default"
            }),
            "{key}={value} from default"
        );
    }
    assert!(settings.entries.iter().all(
        |entry| entry.key != "reasoning_format" && entry.key != "response_continuation_support"
    ));
}

#[test]
fn prefer_continuation_is_responses_only_and_needs_no_support_key() {
    let mut chat_prefer = deployment_file();
    chat_prefer.continuation = Some(Sourced {
        value: "prefer-previous-response-id".to_owned(),
        layer: Layer::Project,
    });
    let error = super::resolve::resolve(&resolvable_cli(), &chat_prefer)
        .expect_err("Chat + prefer fails at resolve");
    assert!(error.0.contains("OpenAI Responses protocol"), "{error:?}");

    let mut responses_prefer = deployment_file();
    responses_prefer.protocol = Some(Sourced {
        value: "openai-responses".to_owned(),
        layer: Layer::Project,
    });
    responses_prefer.continuation = Some(Sourced {
        value: "prefer-previous-response-id".to_owned(),
        layer: Layer::Project,
    });
    let settings = super::resolve::resolve(&resolvable_cli(), &responses_prefer)
        .expect("Responses + prefer is the continuation declaration");
    assert_eq!(
        settings.deployment.continuation_policy,
        ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback
    );
    assert_eq!(settings.deployment.compat, ModelCompat::Compatible);
    assert!(settings.entries.iter().any(|entry| {
        entry.key == "continuation"
            && entry.value == "prefer-previous-response-id"
            && entry.source == "project"
    }));
}

#[test]
fn reasoning_format_is_chat_only_and_shown_when_set() {
    let mut responses = deployment_file();
    responses.protocol = Some(Sourced {
        value: "openai-responses".to_owned(),
        layer: Layer::Project,
    });
    responses.reasoning_format = Some(Sourced {
        value: "none".to_owned(),
        layer: Layer::Project,
    });
    let error = super::resolve::resolve(&resolvable_cli(), &responses)
        .expect_err("reasoning_format is Chat-only");
    assert!(error.0.contains("reasoning_format"), "{error:?}");
    assert!(error.0.contains("OpenAI Chat"), "{error:?}");

    let mut chat = deployment_file();
    chat.reasoning_format = Some(Sourced {
        value: "content-only".to_owned(),
        layer: Layer::Project,
    });
    let settings = super::resolve::resolve(&resolvable_cli(), &chat).expect("Chat format resolves");
    assert_eq!(
        settings.deployment.chat_reasoning_format,
        Some(ChatReasoningFormat::ContentOnly)
    );
    assert!(settings.entries.iter().any(|entry| {
        entry.key == "reasoning_format"
            && entry.value == "content-only"
            && entry.source == "project"
    }));
}

#[test]
fn reasoning_effort_is_validated_against_compat_and_format() {
    assert!(
        validate_reasoning_effort(
            ModelProtocol::OpenAiChat,
            ModelCompat::Compatible,
            None,
            ReasoningEffort::High,
        )
        .is_ok()
    );
    assert!(
        validate_reasoning_effort(
            ModelProtocol::OpenAiChat,
            ModelCompat::Compatible,
            Some(ChatReasoningFormat::EffortOnly),
            ReasoningEffort::Minimal,
        )
        .is_ok()
    );
    assert!(
        validate_reasoning_effort(
            ModelProtocol::OpenAiResponses,
            ModelCompat::Official,
            None,
            ReasoningEffort::VeryHigh,
        )
        .is_ok()
    );

    for format in [ChatReasoningFormat::None, ChatReasoningFormat::ContentOnly] {
        let error = validate_reasoning_effort(
            ModelProtocol::OpenAiChat,
            ModelCompat::Compatible,
            Some(format),
            ReasoningEffort::High,
        )
        .expect_err("none/content-only reject effort");
        assert!(error.0.contains("unsupported by protocol"));
    }
    let error = validate_reasoning_effort(
        ModelProtocol::OpenAiResponses,
        ModelCompat::Compatible,
        None,
        ReasoningEffort::High,
    )
    .expect_err("compatible Responses reject effort");
    assert!(error.0.contains("unsupported by protocol"));
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

#[test]
fn max_parallel_tool_calls_defaults_to_unset_and_accepts_a_positive_file_value() {
    let settings =
        super::resolve::resolve(&resolvable_cli(), &deployment_file()).expect("defaults resolve");
    assert_eq!(settings.max_parallel_tool_calls, None);
    assert!(
        settings
            .entries
            .iter()
            .all(|entry| entry.key != "max_parallel_tool_calls")
    );

    let mut file = deployment_file();
    file.max_parallel_tool_calls = Some(Sourced {
        value: 8,
        layer: Layer::Project,
    });
    let settings = super::resolve::resolve(&resolvable_cli(), &file).expect("resolves");
    assert_eq!(settings.max_parallel_tool_calls, Some(8));
    assert!(settings.entries.iter().any(|entry| {
        entry.key == "max_parallel_tool_calls" && entry.value == "8" && entry.source == "project"
    }));
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
            .expect_err("invalid threshold must fail");
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

fn watch_flags() -> super::watch::ResolveFlags {
    super::watch::ResolveFlags {
        model: None,
        data_dir: None,
        system: None,
        max_tool_rounds: None,
        reasoning_effort: None,
        verbose: false,
        quiet: false,
    }
}

#[test]
fn load_and_resolve_do_not_start_a_watch_task() {
    let _guard = WATCH_TESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(super::watch::active_watch_count(), 0);
    let _ = super::LoadedConfig::load();
    assert_eq!(
        super::watch::active_watch_count(),
        0,
        "single-shot only calls load/resolve and must not start a watch"
    );
}

#[test]
fn watched_paths_match_startup_discovery() {
    let dir = TempDir::new();
    let workspace = dir.0.join("workspace");
    let paths = super::watch::WatchedPaths::from_locations(Some(&dir.0), &workspace);
    assert_eq!(paths.global, Some(dir.0.join("config.toml")));
    assert_eq!(paths.project, workspace.join(".philo").join("config.toml"));
}

#[test]
fn file_stamps_change_when_the_project_toml_appears() {
    let dir = TempDir::new();
    let project = dir.0.join(".philo");
    std::fs::create_dir_all(&project).expect("create project config dir");
    let project_file = project.join("config.toml");
    let paths = super::watch::WatchedPaths {
        global: None,
        project: project_file.clone(),
    };
    let before = super::watch::FileStamps::capture(&paths);
    assert!(before.project.is_none());
    std::fs::write(&project_file, "[ui]\nshow_reasoning = false\n").expect("write");
    let after = super::watch::FileStamps::capture(&paths);
    assert!(after.project.is_some());
    assert_ne!(before, after);
}

#[test]
fn reload_from_layers_applies_project_show_reasoning() {
    let dir = TempDir::new();
    let project = dir.write(
        "project.toml",
        "[deployment]\nmodel = \"m\"\nendpoint = \"https://example.test\"\n\
         [ui]\nshow_reasoning = false\n",
    );
    let (settings, _) =
        super::watch::reload_from_layers(&watch_flags(), None, Some(&project)).expect("reload");
    assert!(!settings.show_reasoning);
}

#[test]
fn corrupt_toml_is_a_reload_error() {
    let dir = TempDir::new();
    let project = dir.write("broken.toml", "[[[not valid");
    match super::watch::reload_from_layers(&watch_flags(), None, Some(&project)) {
        Err(error) => assert!(error.0.contains("invalid TOML")),
        Ok(_) => panic!("corrupt toml must not resolve"),
    }
}

#[test]
fn spawned_watch_is_tracked_until_drop() {
    let _guard = WATCH_TESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = TempDir::new();
    let paths = super::watch::WatchedPaths {
        global: None,
        project: dir.0.join("missing.toml"),
    };
    let task = super::watch::spawn_with(
        paths,
        super::watch::WatchIntervals {
            poll: std::time::Duration::from_millis(20),
            debounce: std::time::Duration::from_millis(5),
        },
        || Err(crate::error::UsageError::new("unused")),
        |_| {},
        || {},
    );
    assert_eq!(super::watch::active_watch_count(), 1);
    drop(task);
    assert_eq!(super::watch::active_watch_count(), 0);
}

fn write_project_toml(path: &std::path::Path, model: &str, stamp: u64) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create project config dir");
    }
    std::fs::write(
        path,
        format!("[deployment]\nmodel = \"{model}\"\nendpoint = \"https://example.test\"\n"),
    )
    .expect("write project toml");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open project toml");
    file.set_modified(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + stamp),
    )
    .expect("bump mtime");
}

#[test]
fn consecutive_config_changes_emit_only_the_latest_candidate() {
    let _guard = WATCH_TESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = TempDir::new();
    let project = dir.0.join(".philo").join("config.toml");
    write_project_toml(&project, "model-a", 1);
    let paths = super::watch::WatchedPaths {
        global: None,
        project: project.clone(),
    };
    let flags = watch_flags();
    let (models_tx, models_rx) = std::sync::mpsc::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (task, driver) = super::watch::spawn_manual(
        paths,
        super::watch::WatchIntervals {
            poll: std::time::Duration::from_millis(20),
            debounce: std::time::Duration::from_millis(5),
        },
        {
            let project = project.clone();
            move || super::watch::reload_from_layers(&flags, None, Some(&project))
        },
        move |result| {
            models_tx
                .send(result.map(|(settings, _)| settings.deployment.model))
                .expect("record candidate");
        },
        move || {
            let _ = ready_tx.send(());
        },
    );

    driver.tick();
    ready_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("watch actor completed the baseline poll");

    write_project_toml(&project, "model-b", 2);
    write_project_toml(&project, "model-c", 3);
    driver.tick();
    driver.tick();
    let model = models_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("latest candidate")
        .expect("reload succeeded");
    assert_eq!(model, "model-c");
    assert!(
        models_rx.try_recv().is_err(),
        "intermediate candidates must not be emitted"
    );
    drop(task);
}
