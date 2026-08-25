use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use clap::Parser;
use philo_agent_runtime::ReasoningEffort;
use philo_model::{ChatReasoningFormat, ModelCompat, ModelContinuationPolicy, ModelProtocol};
use philo_tui::TuiScreen;

use super::file::{FileConfig, Layer, ModelFile, ProviderFile, Sourced, load_layers};
use super::resolve::{
    Verbosity, deployment_for, map_ui_screen, parse_compat, parse_continuation_policy,
    parse_protocol, parse_reasoning_effort, parse_reasoning_format, parse_verbosity,
    validate_reasoning_effort,
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

/// A minimal one-provider catalog TOML used by most resolution tests.
fn catalog_toml(models: &str) -> String {
    format!(
        "[providers.gw]\nendpoint = \"https://example.test\"\n\n[providers.gw.models]\n{models}\n"
    )
}

fn sourced<T>(value: T) -> Sourced<T> {
    Sourced {
        value,
        layer: Layer::Global,
    }
}

/// A hand-built FileConfig carrying one provider (`gw`) with one model
/// (`model-a`) and no other configuration.
fn catalog_file() -> FileConfig {
    let mut file = FileConfig::default();
    let mut provider = ProviderFile {
        endpoint: Some(sourced("https://example.test".to_owned())),
        ..ProviderFile::default()
    };
    provider
        .models
        .insert("model-a".to_owned(), ModelFile::default());
    file.providers.insert("gw".to_owned(), sourced(provider));
    file
}

fn resolvable_cli() -> Cli {
    Cli::try_parse_from(["philo", "hello"]).expect("valid CLI")
}

#[test]
fn absent_files_leave_everything_unset() {
    let config = load_layers(None, None).expect("no layers");
    assert!(config.providers.is_empty());
    assert!(config.warnings.is_empty());
}

#[test]
fn project_layer_replaces_provider_definitions_wholesale() {
    let dir = TempDir::new();
    let global = dir.write(
        "global.toml",
        concat!(
            "[providers.gw]\nendpoint = \"https://global.test\"\n",
            "[providers.other]\nendpoint = \"https://other.test\"\n",
            "[providers.other.models]\nmodel-x = {}\n"
        ),
    );
    let project = dir.write(
        "project.toml",
        "[providers.gw]\nendpoint = \"https://project.test\"\n[providers.gw.models]\nmodel-y = {}\n",
    );

    let config = load_layers(Some(&global), Some(&project)).expect("loads");
    assert_eq!(
        config.providers["gw"].value.endpoint.as_ref().expect("endpoint").value,
        "https://project.test"
    );
    assert_eq!(config.providers["gw"].value.models.len(), 1);
    // Untouched providers survive.
    assert!(config.providers.contains_key("other"));
}

#[test]
fn top_level_data_dir_parses_with_its_layer() {
    let dir = TempDir::new();
    let global = dir.write("global.toml", "data_dir = \"/global/sessions\"\n");
    let project = dir.write("project.toml", "data_dir = \"/project/sessions\"\n");

    let config = load_layers(Some(&global), Some(&project)).expect("loads");
    assert_eq!(
        config.data_dir.expect("data dir"),
        Sourced {
            value: "/project/sessions".to_owned(),
            layer: Layer::Project,
        }
    );
}

#[test]
fn provider_headers_merge_case_insensitively_and_keep_per_header_sources() {
    let dir = TempDir::new();
    let global = dir.write(
        "headers-global.toml",
        "[providers.gw]\nendpoint = \"https://example.test\"\n\
         [providers.gw.headers]\n\"User-Agent\" = \"global-agent\"\n\
         \"X-Route\" = \"global-route\"\n\
         [providers.gw.models]\nmodel-a = {}\n",
    );

    let config = load_layers(Some(&global), None).expect("loads");
    let headers = &config.providers["gw"].value.headers;
    assert_eq!(headers.len(), 2);
    assert_eq!(headers["user-agent"].layer, Layer::Global);
    assert_eq!(headers["x-route"].layer, Layer::Global);

    let settings = super::resolve::resolve(&resolvable_cli(), &config).expect("resolves");
    assert_eq!(
        settings
            .deployment
            .request_headers
            .names()
            .collect::<Vec<_>>(),
        ["user-agent", "x-route"]
    );
    assert!(settings.entries.iter().any(|entry| {
        entry.key == "header.user-agent" && entry.value == "global-agent"
    }));
    assert!(
        settings
            .entries
            .iter()
            .any(|entry| { entry.key == "header.x-route" && entry.value == "<configured>" })
    );
    let debug = format!("{:?}", settings.deployment);
    assert!(!debug.contains("global-route"));
}

#[test]
fn default_user_agent_is_reported_when_no_override_exists() {
    let settings =
        super::resolve::resolve(&resolvable_cli(), &catalog_file()).expect("resolves");
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
        "[providers.gw]\nendpoint = \"https://example.test\"\n\
         [providers.gw.headers]\nAuthorization = \"Bearer do-not-print\"\n\
         [providers.gw.models]\nmodel-a = {}\n",
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
        "data_dir = \"/tmp/sessions\"\n\
         [compaction]\n\
         context_budget = 96000\n\
         auto_threshold = 0.75\n\
         keep_recent_turns = 6\n\
         estimate_bytes_per_token = 4\n\
         [defaults]\n\
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
    assert_eq!(
        config.data_dir.expect("data dir").value,
        "/tmp/sessions"
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
        config.recovery_backoff_base_ms.expect("backoff base").value,
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
        "[providers.gw]\nendpoint = \"https://example.test\"\n[providers.gw.models]\nmodel-a = {}\n\
         [ui]\nverbosity = \"default\"\nfuture = 1\n[telemetry]\nenabled = true\n",
    );
    let config = load_layers(Some(&future), None).expect("unknown keys are tolerated");
    assert_eq!(config.warnings.len(), 2);

    let wrong = dir.write(
        "wrong.toml",
        &catalog_toml("\"model-a\" = { max_output_tokens = \"big\" }\n"),
    );
    assert!(
        load_layers(Some(&wrong), None)
            .expect_err("wrong type")
            .0
            .contains("must be an integer, found string")
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
fn deployment_section_is_a_hard_error_with_migration_text() {
    let dir = TempDir::new();
    let path = dir.write(
        "deployment.toml",
        "[deployment]\nmodel = \"m\"\nendpoint = \"https://example.test\"\n",
    );
    let error = load_layers(Some(&path), None).expect_err("[deployment] is removed");
    assert!(error.0.contains("[deployment] has been removed"), "{error:?}");
    assert!(error.0.contains("[providers.<id>]"), "{error:?}");
    assert!(error.0.contains("[providers.<id>.models]"), "{error:?}");
}

#[test]
fn defaults_generation_keys_are_removed_with_migration_text() {
    let dir = TempDir::new();
    let effort = dir.write(
        "effort.toml",
        "[defaults]\nreasoning_effort = \"high\"\n",
    );
    let error = load_layers(Some(&effort), None).expect_err("removed key");
    assert!(
        error.0.contains("[defaults].reasoning_effort has been removed"),
        "{error:?}"
    );
    assert!(error.0.contains(".reasoning"), "{error:?}");

    let tokens = dir.write(
        "tokens.toml",
        "[defaults]\nmax_output_tokens = 8192\n",
    );
    let error = load_layers(Some(&tokens), None).expect_err("removed key");
    assert!(
        error
            .0
            .contains("[defaults].max_output_tokens has been removed"),
        "{error:?}"
    );
}

#[test]
fn empty_catalog_fails_resolution() {
    let config = FileConfig::default();
    let error =
        super::resolve::resolve(&resolvable_cli(), &config).expect_err("no models configured");
    assert!(error.0.contains("no models configured"), "{error:?}");
}

#[test]
fn unset_or_unknown_models_fall_to_the_first_catalog_entry() {
    // No --model at all: the first id-sorted entry runs.
    let settings =
        super::resolve::resolve(&resolvable_cli(), &catalog_file()).expect("resolves");
    assert_eq!(settings.deployment.model, "gw/model-a");
    assert_eq!(settings.deployment.provider, "gw");
    assert!(
        settings
            .entries
            .iter()
            .any(|entry| entry.key == "model" && entry.value == "gw/model-a")
    );

    // An explicit but unknown name also falls back, with the request recorded.
    let cli = Cli::try_parse_from(["philo", "--model", "gw/nope", "hello"]).expect("valid CLI");
    let settings = super::resolve::resolve(&cli, &catalog_file()).expect("resolves");
    assert_eq!(settings.deployment.model, "gw/model-a");
    assert!(
        settings
            .entries
            .iter()
            .any(|entry| entry.key == "model.requested" && entry.value == "gw/nope")
    );
}

#[test]
fn model_entries_carry_model_capabilities() {
    let dir = TempDir::new();
    let file = dir.write(
        "knobs.toml",
        concat!(
            "[providers.openrouter]\n",
            "endpoint = \"https://openrouter.ai/api/v1\"\n",
            "protocol = \"openai-chat\"\n",
            "api_key_env = \"OPENROUTER_API_KEY\"\n",
            "\n",
            "[providers.openrouter.models]\n",
            "\"anthropic/claude-sonnet-4.5\" = { context_window = 200000, \
             max_output_tokens = 64000, reasoning = [\"low\", \"medium\", \"high\"] }\n",
            "\"openai/gpt-4.1\" = {}\n",
            "\n",
            "[providers.openrouter.headers]\n",
            "\"X-Title\" = \"Philo TUI\"\n",
        ),
    );
    let config = load_layers(Some(&file), None).expect("valid knobs");
    let cli = Cli::try_parse_from([
        "philo",
        "--model",
        "openrouter/anthropic/claude-sonnet-4.5",
        "hello",
    ])
    .expect("valid CLI");
    let settings = super::resolve::resolve(&cli, &config).expect("resolves");

    // The model's window feeds the compaction budget.
    assert_eq!(settings.context_window, Some(200_000));
    let budget_entry = settings
        .entries
        .iter()
        .find(|entry| entry.key == "context_budget")
        .expect("budget entry");
    assert_eq!(budget_entry.value, "200000");
    let model_source = "providers.openrouter.models.anthropic/claude-sonnet-4.5 in the global config";
    assert_eq!(budget_entry.source, model_source);

    // The model cap and middle reasoning tier come from the entry itself.
    assert_eq!(settings.deployment.max_output_tokens, Some(64_000));
    assert_eq!(
        settings.deployment.default_reasoning,
        Some(ReasoningEffort::Medium),
        "the default tier is the middle of the declared set"
    );
    let effort_entry = settings
        .entries
        .iter()
        .find(|entry| entry.key == "reasoning_effort")
        .expect("effort entry");
    assert!(effort_entry.value.contains("medium"), "{}", effort_entry.value);
    assert_eq!(effort_entry.source, model_source);
    let output_entry = settings
        .entries
        .iter()
        .find(|entry| entry.key == "max_output_tokens")
        .expect("output entry");
    assert_eq!(output_entry.value, "64000");
    assert_eq!(output_entry.source, model_source);

    // An explicit flag beats the model default tier.
    let flagged = Cli::try_parse_from([
        "philo",
        "--model",
        "openrouter/anthropic/claude-sonnet-4.5",
        "--reasoning-effort",
        "high",
        "hello",
    ])
    .expect("valid CLI");
    let settings = super::resolve::resolve(&flagged, &config).expect("flagged resolves");
    assert_eq!(settings.reasoning_effort, Some(ReasoningEffort::High));
    assert!(
        settings
            .entries
            .iter()
            .any(|entry| entry.key == "reasoning_effort" && entry.source == "flag")
    );

    // The adopted provider's headers ride along.
    let names: Vec<&str> = settings.deployment.request_headers.names().collect();
    assert!(
        names.contains(&"x-title"),
        "provider header merged: {names:?}"
    );

    // Switching to the other catalog model installs its (empty) parameters.
    let (switched, wire) = deployment_for(&settings, "openrouter/openai/gpt-4.1")
        .expect("known model");
    assert_eq!(wire, "openai/gpt-4.1");
    let switched_names: Vec<&str> = switched.request_headers.names().collect();
    assert!(switched_names.contains(&"x-title"), "{switched_names:?}");
    assert_eq!(switched.max_output_tokens, None);
    assert_eq!(switched.default_reasoning, None);
    assert!(!switched.image_input, "text-only by default");
}

#[test]
fn deployment_for_rejects_unknown_names_instead_of_falling_back() {
    let settings = super::resolve::resolve(&resolvable_cli(), &catalog_file())
        .expect("resolves");
    let error = deployment_for(&settings, "other/model-b").expect_err("unknown model");
    assert!(error.0.contains("unknown model 'other/model-b'"), "{error:?}");
}

#[test]
fn legacy_models_arrays_are_rejected_with_migration_text() {
    let dir = TempDir::new();
    let file = dir.write(
        "legacy.toml",
        concat!(
            "[providers.openai]\n",
            "endpoint = \"https://api.openai.com/v1\"\n",
            "models = [\"gpt-5.2\"]\n",
        ),
    );
    let error = load_layers(Some(&file), None).expect_err("array syntax refused");
    assert!(error.0.contains("must be a table of models"), "{error:?}");
    assert!(error.0.contains("models.<name>"), "{error:?}");
}

#[test]
fn reasoning_tiers_dedupe_sort_and_default_to_the_middle_tier() {
    let mut file = catalog_file();
    let model = ModelFile {
        reasoning: Some(sourced(vec![
            "max".to_owned(),
            "low".to_owned(),
            "max".to_owned(),
            "minimal".to_owned(),
            "high".to_owned(),
        ])),
        ..ModelFile::default()
    };
    let mut provider = ProviderFile {
        endpoint: Some(sourced("https://example.test".to_owned())),
        ..ProviderFile::default()
    };
    provider.models.insert("thinker".to_owned(), model);
    file.providers.insert("gw".to_owned(), sourced(provider));

    let settings = super::resolve::resolve(&resolvable_cli(), &file).expect("resolves");
    let choice = settings
        .models
        .iter()
        .find(|choice| choice.id == "gw/thinker")
        .expect("catalog entry");
    assert_eq!(
        choice.reasoning_tiers,
        vec![
            ReasoningEffort::Minimal,
            ReasoningEffort::Low,
            ReasoningEffort::High,
            ReasoningEffort::Max,
        ],
        "tiers dedupe into canonical order regardless of spelling order"
    );
    assert_eq!(
        choice.default_reasoning,
        Some(ReasoningEffort::High),
        "upper-middle tier of an even set"
    );
    assert_eq!(settings.deployment.default_reasoning, Some(ReasoningEffort::High));

    // A single-tier set defaults to that tier; no declaration means none.
    let plain = super::resolve::resolve(&resolvable_cli(), &catalog_file())
        .expect("plain resolves");
    let choice = plain
        .models
        .iter()
        .find(|choice| choice.id == "gw/model-a")
        .expect("plain entry");
    assert!(choice.reasoning_tiers.is_empty());
    assert_eq!(choice.default_reasoning, None);
}

#[test]
fn flag_reasoning_must_be_a_declared_tier() {
    let dir = TempDir::new();
    let file = dir.write(
        "tiers.toml",
        concat!(
            "[providers.gw]\nendpoint = \"https://example.test\"\n",
            "[providers.gw.models]\n",
            "\"model-a\" = { reasoning = [\"low\", \"high\"] }\n",
        ),
    );
    let config = load_layers(Some(&file), None).expect("parses");

    // Inside the set: accepted.
    let ok = Cli::try_parse_from(["philo", "--reasoning-effort", "high", "hello"])
        .expect("valid CLI");
    let settings = super::resolve::resolve(&ok, &config).expect("declared tier resolves");
    assert_eq!(settings.reasoning_effort, Some(ReasoningEffort::High));

    // Outside the set: rejected with the allowed tiers named.
    let bad = Cli::try_parse_from(["philo", "--reasoning-effort", "xhigh", "hello"])
        .expect("valid CLI");
    let error =
        super::resolve::resolve(&bad, &config).expect_err("undeclared tier must fail");
    assert!(error.0.contains("'xhigh' is not supported"), "{error:?}");
    assert!(error.0.contains("low | high"), "{error:?}");

    // Protocol axes still apply on top of the declared set.
    let incompatible = dir.write(
        "incompatible.toml",
        concat!(
            "[providers.gw]\nendpoint = \"https://example.test\"\n",
            "reasoning_format = \"content-only\"\n",
            "[providers.gw.models]\n",
            "\"model-a\" = { reasoning = [\"high\"] }\n",
        ),
    );
    let config = load_layers(Some(&incompatible), None).expect("parses");
    let error = super::resolve::resolve(&resolvable_cli(), &config)
        .expect_err("content-only cannot encode effort");
    assert!(error.0.contains("unsupported by protocol"), "{error:?}");
}

#[test]
fn flag_reasoning_on_a_non_reasoning_model_is_refused() {
    let cli = Cli::try_parse_from(["philo", "--reasoning-effort", "low", "hello"])
        .expect("valid CLI");
    let error = super::resolve::resolve(&cli, &catalog_file())
        .expect_err("model without tiers refuses effort");
    assert!(error.0.contains("does not support reasoning"), "{error:?}");
    assert!(error.0.contains("gw/model-a"), "{error:?}");
}

#[test]
fn input_modalities_control_image_capability_and_validate_vocabulary() {
    let dir = TempDir::new();
    let file = dir.write(
        "modalities.toml",
        concat!(
            "[providers.gw]\nendpoint = \"https://example.test\"\n",
            "[providers.gw.models]\n",
            "\"vision\" = { input = [\"text\", \"image\"] }\n",
            "\"blind\" = {}\n",
        ),
    );
    let config = load_layers(Some(&file), None).expect("parses");
    let settings = super::resolve::resolve(&resolvable_cli(), &config).expect("resolves");
    let vision = settings
        .models
        .iter()
        .find(|choice| choice.id == "gw/vision")
        .expect("vision entry");
    assert!(vision.image_input);
    let blind = settings
        .models
        .iter()
        .find(|choice| choice.id == "gw/blind")
        .expect("blind entry");
    assert!(!blind.image_input, "input defaults to text-only");

    // An unsupported input modality fails at resolution.
    let audio = dir.write(
        "audio.toml",
        concat!(
            "[providers.gw]\nendpoint = \"https://example.test\"\n",
            "[providers.gw.models]\n",
            "\"talker\" = { input = [\"audio\"] }\n",
        ),
    );
    let config = load_layers(Some(&audio), None).expect("parses");
    let error =
        super::resolve::resolve(&resolvable_cli(), &config).expect_err("unsupported modality");
    assert!(error.0.contains("unsupported modality 'audio'"), "{error:?}");
    assert!(error.0.contains("text | image"), "{error:?}");

    // Output may only declare text today.
    let image_out = dir.write(
        "image-out.toml",
        concat!(
            "[providers.gw]\nendpoint = \"https://example.test\"\n",
            "[providers.gw.models]\n",
            "\"painter\" = { output = [\"image\"] }\n",
        ),
    );
    let config = load_layers(Some(&image_out), None).expect("parses");
    let error =
        super::resolve::resolve(&resolvable_cli(), &config).expect_err("image output unsupported");
    assert!(error.0.contains("unsupported modality 'image'"), "{error:?}");
    assert!(error.0.contains("expected text"), "{error:?}");
}

#[test]
fn provider_cache_policy_resolves_into_choices_and_deployments() {
    use philo_model::CacheRetention;

    let dir = TempDir::new();
    let file = dir.write(
        "cache.toml",
        concat!(
            "[providers.gw]\n",
            "endpoint = \"https://gateway.test/v1\"\n",
            "\n",
            "[providers.gw.cache]\n",
            "retention = \"long\"\n",
            "hints = [\"instructions\", \"history\"]\n",
            "\n",
            "[providers.gw.models]\n",
            "model-a = {}\n",
            "model-b = {}\n",
        ),
    );
    let config = load_layers(Some(&file), None).expect("valid cache config");
    let settings = super::resolve::resolve(&resolvable_cli(), &config).expect("resolves");

    let choice = settings
        .models
        .iter()
        .find(|choice| choice.id == "gw/model-a")
        .expect("catalog entry");
    assert_eq!(choice.cache_policy.retention, CacheRetention::Long);
    assert!(choice.cache_policy.hints.instructions);
    assert!(!choice.cache_policy.hints.tools);
    assert!(choice.cache_policy.hints.history);

    // The active deployment carries the policy; switching carries it too,
    // because the policy belongs to the provider rather than one model.
    assert_eq!(settings.deployment.cache_policy, choice.cache_policy);
    let (switched, _) = deployment_for(&settings, "gw/model-b").expect("known model");
    assert_eq!(switched.cache_policy.retention, CacheRetention::Long);
    assert!(switched.cache_policy.hints.history);
}

#[test]
fn provider_cache_vocabulary_errors_name_their_location() {
    let dir = TempDir::new();
    let bad_retention = dir.write(
        "bad-retention.toml",
        concat!(
            "[providers.gw]\n",
            "endpoint = \"https://gateway.test/v1\"\n",
            "[providers.gw.cache]\n",
            "retention = \"forever\"\n",
            "[providers.gw.models]\n",
            "model-a = {}\n",
        ),
    );
    let config = load_layers(Some(&bad_retention), None).expect("parses");
    let error = super::resolve::resolve(&resolvable_cli(), &config).expect_err("bad retention");
    assert!(
        error.0.contains("invalid cache retention 'forever'"),
        "{error:?}"
    );
    assert!(
        error.0.contains("[providers.gw.cache].retention"),
        "{error:?}"
    );

    let bad_hint = dir.write(
        "bad-hint.toml",
        concat!(
            "[providers.gw]\n",
            "endpoint = \"https://gateway.test/v1\"\n",
            "[providers.gw.cache]\n",
            "hints = [\"tools\", \"kvstore\"]\n",
            "[providers.gw.models]\n",
            "model-a = {}\n",
        ),
    );
    let config = load_layers(Some(&bad_hint), None).expect("parses");
    let error = super::resolve::resolve(&resolvable_cli(), &config).expect_err("bad hint");
    assert!(
        error.0.contains("invalid cache hint 'kvstore'"),
        "{error:?}"
    );
    assert!(error.0.contains("[providers.gw.cache].hints"), "{error:?}");

    let wrong_type = dir.write(
        "wrong-type.toml",
        "[providers.gw]\nendpoint = \"https://gateway.test/v1\"\ncache = \"none\"\n",
    );
    let error = load_layers(Some(&wrong_type), None).expect_err("cache must be a table");
    assert!(error.0.contains("cache must be a table"), "{error:?}");
}

#[test]
fn model_level_params_validate_against_the_provider_axes() {
    let dir = TempDir::new();

    // Unknown reasoning vocabulary fails with the model's location.
    let unknown = dir.write(
        "unknown-effort.toml",
        concat!(
            "[providers.gw]\n",
            "endpoint = \"https://gateway.test/v1\"\n",
            "[providers.gw.models]\n",
            "model-a = { reasoning = [\"extreme\"] }\n",
        ),
    );
    let config = load_layers(Some(&unknown), None).expect("parses");
    let error =
        super::resolve::resolve(&resolvable_cli(), &config).expect_err("unknown effort vocabulary");
    assert!(
        error.0.contains("invalid reasoning effort 'extreme'"),
        "{error:?}"
    );
    assert!(
        error.0.contains("[providers.gw.models.model-a].reasoning entry 'extreme'"),
        "{error:?}"
    );

    // Out-of-range output caps fail too.
    let huge = dir.write(
        "huge.toml",
        concat!(
            "[providers.gw]\n",
            "endpoint = \"https://gateway.test/v1\"\n",
            "[providers.gw.models]\n",
            "model-a = { max_output_tokens = 99999999999 }\n",
        ),
    );
    let config = load_layers(Some(&huge), None).expect("parses");
    let error = super::resolve::resolve(&resolvable_cli(), &config).expect_err("cap overflow");
    assert!(
        error
            .0
            .contains("[providers.gw.models.model-a].max_output_tokens is out of range"),
        "{error:?}"
    );

    // Non-table entries and secret-looking names are refused at parse time.
    let scalar = dir.write(
        "scalar.toml",
        "[providers.gw]\nendpoint = \"https://gateway.test/v1\"\nmodels = { a = 1 }\n",
    );
    let error = load_layers(Some(&scalar), None).expect_err("scalar model entry");
    assert!(error.0.contains("must be a table"), "{error:?}");

    let secret_named = dir.write(
        "secret-named.toml",
        "[providers.gw]\nendpoint = \"https://gateway.test/v1\"\n[providers.gw.models]\ntoken = {}\n",
    );
    let error = load_layers(Some(&secret_named), None).expect_err("secret-like name");
    assert!(error.0.contains("would store a secret"), "{error:?}");
}

#[test]
fn secret_values_are_refused_without_echoing_them() {
    let dir = TempDir::new();
    let token = dir.write(
        "token.toml",
        "[providers.gw]\ntoken = \"sk-not-a-real-key\"\n",
    );
    let error = load_layers(Some(&token), None).expect_err("secret key refused");
    assert!(error.0.contains("would store a secret"));
    assert!(!error.0.contains("sk-not-a-real-key"));

    let misplaced = dir.write(
        "misplaced.toml",
        "[providers.gw]\napi_key_env = \"sk-must-not-render\"\n",
    );
    let error = load_layers(Some(&misplaced), None).expect_err("not an env name");
    assert!(error.0.contains("environment variable name"));
    assert!(!error.0.contains("sk-must-not-render"));

    let malformed = dir.write(
        "malformed.toml",
        "[providers.gw]\napi_key_env = \"sk-must-not-leak\n",
    );
    let error = load_layers(Some(&malformed), None).expect_err("invalid TOML");
    assert!(error.0.contains("invalid TOML"));
    assert!(!error.0.contains("sk-must-not-leak"));

    // api_key stays sanctioned only inside [providers.<id>].
    let stray = dir.write("stray.toml", "[unknown]\napi_key = \"sk-not-a-real-key\"\n");
    let error = load_layers(Some(&stray), None).expect_err("secret outside providers");
    assert!(error.0.contains("would store a secret"));
    assert!(!error.0.contains("sk-not-a-real-key"));

    let header = dir.write(
        "header.toml",
        "[providers.gw.headers]\napi_key = \"sk-not-a-real-key\"\n",
    );
    let error = load_layers(Some(&header), None).expect_err("secret header refused");
    assert!(error.0.contains("would store a secret"));
    assert!(!error.0.contains("sk-not-a-real-key"));
}

#[test]
fn literal_api_keys_parse_redacted_in_providers_only() {
    let dir = TempDir::new();
    let file = dir.write(
        "keys.toml",
        concat!(
            "[providers.openai]\n",
            "endpoint = \"https://api.openai.com/v1\"\n",
            "api_key = \"sk-provider-secret\"\n",
            "\n",
            "[providers.openai.models]\n",
            "\"gpt-5.2\" = {}\n",
        ),
    );
    let config = load_layers(Some(&file), None).expect("valid config");
    assert_eq!(
        config.providers["openai"]
            .value
            .api_key
            .as_ref()
            .expect("provider key")
            .value
            .0,
        "sk-provider-secret"
    );
    // Neither the parsed config nor its debug output carries the secret.
    let debug = format!("{config:?}");
    assert!(!debug.contains("sk-provider-secret"), "{debug}");
    assert!(config.warnings.is_empty(), "the global layer warns not");
}

#[test]
fn api_key_and_api_key_env_are_mutually_exclusive() {
    let dir = TempDir::new();
    let provider = dir.write(
        "provider.toml",
        concat!(
            "[providers.openai]\n",
            "endpoint = \"https://api.openai.com/v1\"\n",
            "api_key = \"a\"\n",
            "api_key_env = \"B\"\n",
            "\n",
            "[providers.openai.models]\n",
            "\"gpt-5.2\" = {}\n",
        ),
    );
    let file = load_layers(Some(&provider), None).expect("both keys parse");
    let error = super::resolve::resolve(&resolvable_cli(), &file).expect_err("provider conflict");
    assert!(
        error
            .0
            .contains("[providers.openai] sets both api_key and api_key_env")
    );
}

#[test]
fn project_layer_literal_keys_warn() {
    let dir = TempDir::new();
    let project = dir.write(
        "project.toml",
        concat!(
            "[providers.deepseek]\n",
            "endpoint = \"https://api.deepseek.com/v1\"\n",
            "api_key = \"sk-project-secret\"\n",
            "\n",
            "[providers.deepseek.models]\n",
            "deepseek-chat = {}\n",
        ),
    );
    let config = load_layers(None, Some(&project)).expect("layers combine");
    assert!(
        config
            .warnings
            .iter()
            .any(|warning| warning.contains("[providers.deepseek].api_key")),
        "{:?}",
        config.warnings
    );

    let quiet = dir.write("quiet.toml", "[ui]\nverbosity = \"default\"\n");
    let config = load_layers(None, Some(&quiet)).expect("no literal keys");
    assert!(
        !config
            .warnings
            .iter()
            .any(|warning| warning.contains("api_key")),
        "{:?}",
        config.warnings
    );
}

#[test]
fn value_parsers_cover_the_supported_vocabulary() {
    for (text, expected) in [
        ("minimal", ReasoningEffort::Minimal),
        ("low", ReasoningEffort::Low),
        ("medium", ReasoningEffort::Medium),
        ("high", ReasoningEffort::High),
        ("xhigh", ReasoningEffort::Xhigh),
        ("max", ReasoningEffort::Max),
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
        super::resolve::resolve(&resolvable_cli(), &catalog_file()).expect("defaults resolve");
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
        let mut file = catalog_file();
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
    let mut file = catalog_file();
    file.screen = Some(Sourced {
        value: "fullscreen".to_owned(),
        layer: Layer::Global,
    });
    let error =
        super::resolve::resolve(&resolvable_cli(), &file).expect_err("invalid screen must fail");
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

    let mut file = catalog_file();
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
        super::resolve::resolve(&resolvable_cli(), &catalog_file()).expect("defaults resolve");
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
fn provider_defaults_are_chat_compatible_and_stateless() {
    let settings =
        super::resolve::resolve(&resolvable_cli(), &catalog_file()).expect("defaults resolve");
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
    assert!(settings.entries.iter().all(|entry| entry.key != "reasoning_format"));
}

#[test]
fn prefer_continuation_is_responses_only_at_provider_level() {
    let dir = TempDir::new();

    let chat_prefer = dir.write(
        "chat-prefer.toml",
        concat!(
            "[providers.gw]\nendpoint = \"https://example.test\"\n",
            "continuation = \"prefer-previous-response-id\"\n",
            "[providers.gw.models]\nmodel-a = {}\n",
        ),
    );
    let config = load_layers(Some(&chat_prefer), None).expect("parses");
    let error = super::resolve::resolve(&resolvable_cli(), &config)
        .expect_err("Chat + prefer fails at resolve");
    assert!(error.0.contains("OpenAI Responses protocol"), "{error:?}");
    assert!(error.0.contains("[providers.gw]"), "{error:?}");

    let responses_prefer = dir.write(
        "responses-prefer.toml",
        concat!(
            "[providers.gw]\nendpoint = \"https://example.test\"\n",
            "protocol = \"openai-responses\"\n",
            "continuation = \"prefer-previous-response-id\"\n",
            "[providers.gw.models]\nmodel-a = {}\n",
        ),
    );
    let config = load_layers(Some(&responses_prefer), None).expect("parses");
    let settings = super::resolve::resolve(&resolvable_cli(), &config)
        .expect("Responses + prefer is the continuation declaration");
    assert_eq!(
        settings.deployment.continuation_policy,
        ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback
    );
    assert!(settings.entries.iter().any(|entry| {
        entry.key == "continuation"
            && entry.value == "prefer-previous-response-id"
            && entry.source.contains("providers.gw")
    }));
}

#[test]
fn reasoning_format_is_chat_only_and_shown_when_set() {
    let dir = TempDir::new();

    let responses = dir.write(
        "responses-format.toml",
        concat!(
            "[providers.gw]\nendpoint = \"https://example.test\"\n",
            "protocol = \"openai-responses\"\n",
            "reasoning_format = \"none\"\n",
            "[providers.gw.models]\nmodel-a = {}\n",
        ),
    );
    let config = load_layers(Some(&responses), None).expect("parses");
    let error = super::resolve::resolve(&resolvable_cli(), &config)
        .expect_err("reasoning_format is Chat-only");
    assert!(error.0.contains("reasoning_format"), "{error:?}");
    assert!(error.0.contains("OpenAI Chat"), "{error:?}");

    let chat = dir.write(
        "chat-format.toml",
        concat!(
            "[providers.gw]\nendpoint = \"https://example.test\"\n",
            "reasoning_format = \"content-only\"\n",
            "[providers.gw.models]\nmodel-a = {}\n",
        ),
    );
    let config = load_layers(Some(&chat), None).expect("parses");
    let settings = super::resolve::resolve(&resolvable_cli(), &config).expect("Chat format resolves");
    assert_eq!(
        settings.deployment.chat_reasoning_format,
        Some(ChatReasoningFormat::ContentOnly)
    );
    assert!(settings.entries.iter().any(|entry| {
        entry.key == "reasoning_format"
            && entry.value == "content-only"
            && entry.source.contains("providers.gw")
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
            ReasoningEffort::Xhigh,
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
        "gw/model-a",
        "--reasoning-effort",
        "high",
        "hello",
    ])
    .expect("valid CLI");

    // The flag must name a tier the model declares.
    let mut file = catalog_file();
    if let Some(provider) = file.providers.get_mut("gw")
        && let Some(model) = provider.value.models.get_mut("model-a")
    {
        model.reasoning = Some(Sourced {
            value: vec!["high".to_owned()],
            layer: Layer::Global,
        });
    }

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
        super::resolve::resolve(&resolvable_cli(), &catalog_file()).expect("defaults resolve");
    assert_eq!(settings.max_parallel_tool_calls, None);
    assert!(
        settings
            .entries
            .iter()
            .all(|entry| entry.key != "max_parallel_tool_calls")
    );

    let mut file = catalog_file();
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

#[test]
fn explicit_compaction_values_override_the_model_window() {
    let mut file = catalog_file();
    if let Some(provider) = file.providers.get_mut("gw")
        && let Some(model) = provider.value.models.get_mut("model-a")
    {
        model.context_window = Some(Sourced {
            value: 128_000,
            layer: Layer::Global,
        });
    }
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
fn model_context_window_is_the_budget_fallback_and_defaults_are_stable() {
    let mut file = catalog_file();
    if let Some(provider) = file.providers.get_mut("gw")
        && let Some(model) = provider.value.models.get_mut("model-a")
    {
        model.context_window = Some(Sourced {
            value: 128_000,
            layer: Layer::Global,
        });
    }

    let settings = super::resolve::resolve(&resolvable_cli(), &file).expect("resolves");
    assert_eq!(settings.compaction.context_budget, Some(128_000));
    assert_eq!(
        settings.compaction,
        philo_agent_runtime::CompactionConfig {
            context_budget: Some(128_000),
            ..philo_agent_runtime::CompactionConfig::default()
        }
    );
    assert!(
        settings
            .entries
            .iter()
            .any(|entry| entry.key == "context_budget"
                && entry.value == "128000"
                && entry.source.contains("providers.gw.models.model-a"))
    );
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

    // Without a model window there is no budget: automatic compaction stays off.
    let settings =
        super::resolve::resolve(&resolvable_cli(), &catalog_file()).expect("resolves");
    assert_eq!(settings.compaction.context_budget, None);
    assert!(settings.entries.iter().any(|entry| {
        entry.key == "context_budget" && entry.value == "none" && entry.source == "default"
    }));
}

#[test]
fn compaction_threshold_must_be_a_finite_ratio() {
    for value in [0.0, -0.1, 1.01, f64::NAN, f64::INFINITY] {
        let mut file = catalog_file();
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
        "[providers.gw]\nendpoint = \"https://example.test\"\n\
         [providers.gw.models]\nmodel-a = {}\n\
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

fn write_project_toml(path: &std::path::Path, endpoint_host: &str, stamp: u64) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create project config dir");
    }
    std::fs::write(
        path,
        format!(
            "[providers.gw]\nendpoint = \"https://{endpoint_host}\"\n\
             [providers.gw.models]\nmodel-a = {{}}\n"
        ),
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
    write_project_toml(&project, "host-a.test", 1);
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
                .send(result.map(|(settings, _)| settings.deployment.endpoint))
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

    write_project_toml(&project, "host-b.test", 2);
    write_project_toml(&project, "host-c.test", 3);
    driver.tick();
    driver.tick();
    let endpoint = models_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("latest candidate")
        .expect("reload succeeded");
    assert_eq!(endpoint, "https://host-c.test");
    assert!(
        models_rx.try_recv().is_err(),
        "intermediate candidates must not be emitted"
    );
    drop(task);
}
