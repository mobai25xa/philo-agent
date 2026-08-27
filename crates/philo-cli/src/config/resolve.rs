//! Effective run settings: flag > environment > project file > global file
//! > built-in/profile default.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use philo_agent_runtime::{CompactionConfig, ReasoningEffort, RecoveryConfig};
use philo_model::{
    CacheRetention, ChatReasoningFormat, DEFAULT_USER_AGENT, ModelCachePolicy, ModelCompat,
    ModelContinuationPolicy, ModelProtocol, ModelRequestHeaders, PromptCacheHints,
};

use super::file::{FileConfig, FileSecret, Sourced};
use crate::args::Cli;
use crate::error::UsageError;

const API_KEY_ENV: &str = "PHILO_API_KEY";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verbosity {
    Default,
    Verbose,
    Quiet,
}

/// One selectable model advertised to the service's `ListModels`: a stable
/// composite id plus the deployment parameters used to assemble it. Catalog
/// entries are the final source of truth for every per-model capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelChoice {
    /// Stable install identity: `{provider_id}/{model}`.
    pub id: String,
    /// Owning provider id.
    pub provider_id: String,
    /// Wire-level model name within the provider.
    pub model: String,
    pub endpoint: String,
    pub protocol: ModelProtocol,
    pub credential: Credential,
    pub compat: ModelCompat,
    pub chat_reasoning_format: Option<ChatReasoningFormat>,
    pub continuation_policy: ModelContinuationPolicy,
    /// Context window this model declares; feeds the compaction budget.
    pub context_window: Option<u64>,
    /// Canonical header name -> value, validated at resolution. Sent with
    /// every request to the owning provider.
    pub request_headers: Vec<(String, String)>,
    /// Output cap declared by this model.
    pub max_output_tokens: Option<u32>,
    /// Reasoning tiers this model supports, in canonical effort order.
    /// Empty means the model does not support reasoning at all.
    pub reasoning_tiers: Vec<ReasoningEffort>,
    /// Default reasoning tier: the middle of the declared set.
    pub default_reasoning: Option<ReasoningEffort>,
    /// Whether this model accepts image input parts.
    pub image_input: bool,
    /// Cache identity policy shared by every request to this provider.
    pub cache_policy: ModelCachePolicy,
}

/// Where a deployment's API key comes from. The literal form only exists for
/// config-file credentials; diagnostics render it as `<configured>` and never
/// echo the value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Credential {
    /// Name of the environment variable carrying the key.
    EnvName(String),
    /// The key itself, already redacted everywhere except adapter assembly.
    Literal(String),
}

impl fmt::Display for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EnvName(name) => formatter.write_str(name),
            Self::Literal(_) => formatter.write_str("<configured>"),
        }
    }
}

/// Model deployment configuration owned by the composition root. Always
/// derived from a catalog `ModelChoice`; there is no out-of-catalog fallback.
#[derive(Clone, Debug)]
pub struct Deployment {
    pub provider: String,
    pub protocol: ModelProtocol,
    pub model: String,
    pub endpoint: String,
    /// Environment-variable name or literal key; never rendered verbatim.
    pub credential: Credential,
    pub request_headers: ModelRequestHeaders,
    pub compat: ModelCompat,
    pub chat_reasoning_format: Option<ChatReasoningFormat>,
    pub continuation_policy: ModelContinuationPolicy,
    /// Output cap declared by the active model.
    pub max_output_tokens: Option<u32>,
    /// Default reasoning tier of the active model (middle of its tier set).
    pub default_reasoning: Option<ReasoningEffort>,
    /// Whether the active model accepts image input parts.
    pub image_input: bool,
    /// Cache identity policy for this provider's requests.
    pub cache_policy: ModelCachePolicy,
    /// Transport-level response-head deadline (`[recovery]`), applied at
    /// assembly time.
    pub response_head_timeout: Option<Duration>,
    /// Transport-level stream-idle deadline (`[recovery]`), applied at
    /// assembly time.
    pub stream_idle_timeout: Option<Duration>,
}

/// One effective non-secret setting shown through the interactive `/config`
/// command. Conversion into TUI vocabulary happens in the TUI host adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveSetting {
    pub key: String,
    pub value: String,
    pub source: String,
}

/// Everything one run needs.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Deployment of the active model, always derived from the catalog.
    pub deployment: Deployment,
    /// Selectable models across configured providers, id-sorted. Never empty:
    /// resolution fails when the catalog has no models at all.
    pub models: Vec<ModelChoice>,
    /// Alias name -> composite model id, in alias order.
    pub aliases: Vec<(String, String)>,
    pub data_dir: PathBuf,
    /// Context window of the active model; feeds the compaction budget.
    pub context_window: Option<u64>,
    pub compaction: CompactionConfig,
    /// Explicit reasoning override (`--reasoning-effort`); `None` lets the
    /// active model's default tier apply.
    pub reasoning_effort: Option<ReasoningEffort>,
    pub max_tool_rounds: Option<u32>,
    pub max_parallel_tool_calls: Option<u32>,
    pub operation_timeout: Option<Duration>,
    pub shell_timeout_secs: Option<u64>,
    /// Turn-engine model-call recovery policy.
    pub recovery: RecoveryConfig,
    pub verbosity: Verbosity,
    pub show_reasoning: bool,
    pub entries: Vec<EffectiveSetting>,
}

/// One resolved value and the layer that supplied it.
struct Pick<T> {
    value: T,
    source: &'static str,
}

fn from_file<T: Clone>(file: Option<&Sourced<T>>) -> Option<Pick<T>> {
    file.map(|sourced| Pick {
        value: sourced.value.clone(),
        source: sourced.layer.name(),
    })
}

/// flag > env > project > global. `None` leaves the profile default in place.
fn pick_string(
    flag: Option<&str>,
    env_var: Option<&str>,
    file: Option<&Sourced<String>>,
) -> Option<Pick<String>> {
    if let Some(value) = flag {
        return Some(Pick {
            value: value.to_owned(),
            source: "flag",
        });
    }
    if let Some(value) = env_var.and_then(|name| std::env::var(name).ok()) {
        return Some(Pick {
            value,
            source: "env",
        });
    }
    from_file(file)
}

fn origin(source: &str, flag: Option<&str>, env: Option<&str>, key: &str) -> String {
    match source {
        "flag" => flag.unwrap_or("the command line").to_owned(),
        "env" => env.unwrap_or("the environment").to_owned(),
        layer => format!("{key} in the {layer} config"),
    }
}

fn pick_integer(flag: Option<i64>, file: Option<&Sourced<i64>>) -> Option<Pick<i64>> {
    if let Some(value) = flag {
        return Some(Pick {
            value,
            source: "flag",
        });
    }
    from_file(file)
}

pub(super) fn resolve(cli: &Cli, file: &FileConfig) -> Result<Settings, UsageError> {
    let mut entries: Vec<EffectiveSetting> = Vec::new();
    let mut record = |key: &str, value: String, source: &str| {
        entries.push(EffectiveSetting {
            key: key.to_owned(),
            value,
            source: source.to_owned(),
        });
    };

    // The provider catalog is the only source of models. Resolution fails
    // when it is empty: philo no longer carries an out-of-catalog fallback.
    let models = model_choices(file)?;
    if models.is_empty() {
        return Err(UsageError::new(
            "no models configured: declare at least one [providers.<id>] section with a \
             [providers.<id>.models] entry",
        ));
    }
    for (id, sourced) in &file.providers {
        record("providers", id.clone(), sourced.layer.name());
    }
    let aliases = resolve_aliases(file, &models)?;
    for (name, sourced) in &file.aliases {
        record(
            "aliases",
            format!("{name} -> {}", sourced.value),
            sourced.layer.name(),
        );
    }

    // Active model: flag/env name resolved through aliases and the catalog;
    // unset or unmatched names fall to the first catalog entry.
    let mut requested_model = pick_string(cli.model.as_deref(), Some("PHILO_MODEL"), None);
    if let Some(picked) = &mut requested_model
        && let Some((_, target)) = aliases.iter().find(|(name, _)| *name == picked.value)
    {
        record("model.alias", picked.value.clone(), "aliases");
        picked.value = target.clone();
    }
    let adopted = requested_model
        .as_ref()
        .and_then(|picked| models.iter().find(|choice| choice.id == picked.value))
        .unwrap_or_else(|| models.first().expect("catalog is non-empty"));
    match &requested_model {
        Some(picked) if picked.value == adopted.id => {
            record("model", adopted.id.clone(), picked.source);
        }
        Some(picked) => {
            // The request named something outside the catalog; the first
            // catalog entry runs instead.
            record("model.requested", picked.value.clone(), picked.source);
            record("model", adopted.id.clone(), "default");
        }
        None => {
            record("model", adopted.id.clone(), "default");
        }
    }
    let choice_layer = file
        .providers
        .get(&adopted.provider_id)
        .map(|sourced| sourced.layer.name())
        .unwrap_or("default");

    let (data_dir, data_dir_source) = resolve_data_dir(cli.data_dir.clone(), file)?;
    record("data_dir", data_dir.display().to_string(), data_dir_source);

    // Protocol-stack rows follow the adopted provider; the catalog supplies
    // them or the built-in defaults stand.
    let provider_origin =
        format!("[providers.{}] in the {choice_layer} config", adopted.provider_id);
    let declared = |declared: bool| -> String {
        if declared {
            provider_origin.clone()
        } else {
            "default".to_owned()
        }
    };
    record("endpoint", adopted.endpoint.clone(), &provider_origin);
    let protocol_source = declared(
        file.providers
            .get(&adopted.provider_id)
            .is_some_and(|sourced| sourced.value.protocol.is_some()),
    );
    record("protocol", adopted.protocol.as_str().to_owned(), &protocol_source);
    let compat_source = declared(
        file.providers
            .get(&adopted.provider_id)
            .is_some_and(|sourced| sourced.value.compat.is_some()),
    );
    record("compat", adopted.compat.as_str().to_owned(), &compat_source);
    if let Some(format) = adopted.chat_reasoning_format {
        record("reasoning_format", format.as_str().to_owned(), &provider_origin);
    }
    let continuation_source = declared(
        file.providers
            .get(&adopted.provider_id)
            .is_some_and(|sourced| sourced.value.continuation.is_some()),
    );
    record(
        "continuation",
        continuation_label(adopted.continuation_policy).to_owned(),
        &continuation_source,
    );

    let context_window = adopted.context_window;
    if let Some(window) = context_window {
        record(
            "context_window",
            window.to_string(),
            &format!(
                "providers.{}.models.{} in the {choice_layer} config",
                adopted.provider_id, adopted.model
            ),
        );
    }

    let context_budget = match from_file(file.compaction_context_budget.as_ref()) {
        Some(picked) => {
            record("context_budget", picked.value.to_string(), picked.source);
            Some(positive_u64("[compaction].context_budget", picked.value)?)
        }
        None => match context_window {
            Some(value) => {
                record(
                    "context_budget",
                    value.to_string(),
                    &format!(
                        "providers.{}.models.{} in the {choice_layer} config",
                        adopted.provider_id, adopted.model
                    ),
                );
                Some(value)
            }
            None => {
                record("context_budget", "none".to_owned(), "default");
                None
            }
        },
    };

    let auto_threshold = match from_file(file.compaction_auto_threshold.as_ref()) {
        Some(picked) => {
            let value = threshold_f32("[compaction].auto_threshold", picked.value)?;
            record("auto_threshold", value.to_string(), picked.source);
            value
        }
        None => {
            let value = CompactionConfig::default().auto_threshold;
            record("auto_threshold", value.to_string(), "default");
            value
        }
    };

    let keep_recent_turns = match from_file(file.compaction_keep_recent_turns.as_ref()) {
        Some(picked) => {
            record("keep_recent_turns", picked.value.to_string(), picked.source);
            positive_u32("[compaction].keep_recent_turns", picked.value)?
        }
        None => {
            let value = CompactionConfig::default().keep_recent_turns;
            record("keep_recent_turns", value.to_string(), "default");
            value
        }
    };

    let estimate_bytes_per_token =
        match from_file(file.compaction_estimate_bytes_per_token.as_ref()) {
            Some(picked) => {
                record(
                    "estimate_bytes_per_token",
                    picked.value.to_string(),
                    picked.source,
                );
                positive_u32("[compaction].estimate_bytes_per_token", picked.value)?
            }
            None => {
                let value = CompactionConfig::default().estimate_bytes_per_token;
                record("estimate_bytes_per_token", value.to_string(), "default");
                value
            }
        };

    // An explicit flag is the only override above the model's own tier set:
    // it must name a tier the active model declares, and the protocol stack
    // must be able to encode it.
    let reasoning_effort = match cli.reasoning_effort.as_deref() {
        Some(value) => {
            record("reasoning_effort", value.to_owned(), "flag");
            let effort = parse_reasoning_effort(value)
                .map_err(|error| error.at("--reasoning-effort"))?;
            validate_reasoning_tiers(adopted, effort).map_err(|error| error.at("--reasoning-effort"))?;
            validate_reasoning_effort(
                adopted.protocol,
                adopted.compat,
                adopted.chat_reasoning_format,
                effort,
            )
            .map_err(|error| error.at("--reasoning-effort"))?;
            Some(effort)
        }
        None => None,
    };
    if reasoning_effort.is_none() && let Some(effort) = adopted.default_reasoning {
        record(
            "reasoning_effort",
            format!(
                "{} (middle of {})",
                effort_label(effort),
                tiers_label(&adopted.reasoning_tiers)
            ),
            &format!(
                "providers.{}.models.{} in the {choice_layer} config",
                adopted.provider_id, adopted.model
            ),
        );
    }

    let max_tool_rounds = match pick_integer(
        cli.max_tool_rounds.map(i64::from),
        file.max_tool_rounds.as_ref(),
    ) {
        Some(picked) => {
            record("max_tool_rounds", picked.value.to_string(), picked.source);
            Some(u32::try_from(picked.value).map_err(|_| {
                UsageError::new(format!("max_tool_rounds is out of range: {}", picked.value))
            })?)
        }
        None => None,
    };

    if let Some(tokens) = adopted.max_output_tokens {
        record(
            "max_output_tokens",
            tokens.to_string(),
            &format!(
                "providers.{}.models.{} in the {choice_layer} config",
                adopted.provider_id, adopted.model
            ),
        );
    }

    let max_parallel_tool_calls = match pick_integer(None, file.max_parallel_tool_calls.as_ref()) {
        Some(picked) => {
            record(
                "max_parallel_tool_calls",
                picked.value.to_string(),
                picked.source,
            );
            Some(u32::try_from(picked.value).map_err(|_| {
                UsageError::new(format!(
                    "max_parallel_tool_calls is out of range: {}",
                    picked.value
                ))
            })?)
        }
        None => None,
    };

    let operation_timeout = match from_file(file.operation_timeout_secs.as_ref()) {
        Some(picked) => {
            record(
                "operation_timeout_secs",
                picked.value.to_string(),
                picked.source,
            );
            Some(Duration::from_secs(positive_u64(
                "[defaults].operation_timeout_secs",
                picked.value,
            )?))
        }
        None => None,
    };

    let shell_timeout_secs = match from_file(file.shell_timeout_secs.as_ref()) {
        Some(picked) => {
            record(
                "shell_timeout_secs",
                picked.value.to_string(),
                picked.source,
            );
            Some(positive_u64("[tools].shell_timeout_secs", picked.value)?)
        }
        None => None,
    };

    let recovery = RecoveryConfig {
        enabled: match from_file(file.recovery_enabled.as_ref()) {
            Some(picked) => {
                record("recovery.enabled", picked.value.to_string(), picked.source);
                picked.value
            }
            None => {
                record("recovery.enabled", "true".to_owned(), "default");
                true
            }
        },
        max_retries: match from_file(file.recovery_max_retries.as_ref()) {
            Some(picked) => {
                record(
                    "recovery.max_retries",
                    picked.value.to_string(),
                    picked.source,
                );
                u32::try_from(picked.value).map_err(|_| {
                    UsageError::new(format!(
                        "recovery.max_retries is out of range: {}",
                        picked.value
                    ))
                })?
            }
            None => {
                record("recovery.max_retries", "3".to_owned(), "default");
                3
            }
        },
        backoff_base_ms: match from_file(file.recovery_backoff_base_ms.as_ref()) {
            Some(picked) => {
                record(
                    "recovery.backoff_base_ms",
                    picked.value.to_string(),
                    picked.source,
                );
                positive_u64("[recovery].backoff_base_ms", picked.value)?
            }
            None => 500,
        },
        backoff_max_ms: match from_file(file.recovery_backoff_max_ms.as_ref()) {
            Some(picked) => {
                record(
                    "recovery.backoff_max_ms",
                    picked.value.to_string(),
                    picked.source,
                );
                positive_u64("[recovery].backoff_max_ms", picked.value)?
            }
            None => 8_000,
        },
    };

    let response_head_timeout = match from_file(file.recovery_response_head_timeout_secs.as_ref()) {
        Some(picked) => {
            record(
                "recovery.response_head_timeout_secs",
                picked.value.to_string(),
                picked.source,
            );
            let seconds = positive_u64("[recovery].response_head_timeout_secs", picked.value)?;
            (seconds > 0).then_some(Duration::from_secs(seconds))
        }
        None => None,
    };

    let stream_idle_timeout = match from_file(file.recovery_stream_idle_timeout_secs.as_ref()) {
        Some(picked) => {
            record(
                "recovery.stream_idle_timeout_secs",
                picked.value.to_string(),
                picked.source,
            );
            let seconds = positive_u64("[recovery].stream_idle_timeout_secs", picked.value)?;
            (seconds > 0).then_some(Duration::from_secs(seconds))
        }
        None => None,
    };

    let verbosity = if cli.quiet {
        record("verbosity", "quiet".to_owned(), "flag");
        Verbosity::Quiet
    } else if cli.verbose {
        record("verbosity", "verbose".to_owned(), "flag");
        Verbosity::Verbose
    } else {
        match from_file(file.verbosity.as_ref()) {
            Some(picked) => {
                record("verbosity", picked.value.clone(), picked.source);
                parse_verbosity(&picked.value).map_err(|error| {
                    error.at(&origin(picked.source, None, None, "[ui].verbosity"))
                })?
            }
            None => {
                record("verbosity", "default".to_owned(), "default");
                Verbosity::Default
            }
        }
    };

    let show_reasoning = match from_file(file.show_reasoning.as_ref()) {
        Some(picked) => {
            record("show_reasoning", picked.value.to_string(), picked.source);
            picked.value
        }
        None => {
            record("show_reasoning", "true".to_owned(), "default");
            true
        }
    };

    record(
        "provider_config",
        adopted.provider_id.clone(),
        choice_layer,
    );
    entries.extend(header_entries(adopted));

    let deployment = deployment_from_choice(
        adopted,
        response_head_timeout,
        stream_idle_timeout,
    );

    Ok(Settings {
        deployment,
        models,
        aliases,
        data_dir,
        context_window,
        compaction: CompactionConfig {
            context_budget,
            auto_threshold,
            keep_recent_turns,
            estimate_bytes_per_token,
        },
        reasoning_effort,
        max_tool_rounds,
        max_parallel_tool_calls,
        operation_timeout,
        shell_timeout_secs,
        recovery,
        verbosity,
        show_reasoning,
        entries,
    })
}

/// Flattens `[providers.<id>]` sections into selectable model choices,
/// id-sorted for a stable `ListModels` order.
fn model_choices(file: &FileConfig) -> Result<Vec<ModelChoice>, UsageError> {
    let mut choices = Vec::new();
    for (id, sourced) in &file.providers {
        let provider = &sourced.value;
        let layer = sourced.layer.name();
        let endpoint = provider.endpoint.as_ref().ok_or_else(|| {
            UsageError::new(format!(
                "[providers.{id}] needs an endpoint in the {layer} config"
            ))
        })?;
        let protocol = match provider.protocol.as_ref() {
            Some(picked) => parse_protocol(&picked.value).map_err(|error| {
                error.at(&format!("[providers.{id}].protocol in the {layer} config"))
            })?,
            None => ModelProtocol::OpenAiChat,
        };
        let compat = match provider.compat.as_ref() {
            Some(picked) => parse_compat(&picked.value).map_err(|error| {
                error.at(&format!("[providers.{id}].compat in the {layer} config"))
            })?,
            None => ModelCompat::Compatible,
        };
        let chat_reasoning_format = match provider.reasoning_format.as_ref() {
            Some(picked) => Some(parse_reasoning_format(&picked.value).map_err(|error| {
                error.at(&format!(
                    "[providers.{id}].reasoning_format in the {layer} config"
                ))
            })?),
            None => None,
        };
        let credential = resolve_credential(
            &format!("[providers.{id}]"),
            provider.api_key.as_ref(),
            provider.api_key_env.as_ref(),
            &mut |_, _, _| {},
        )?;
        let continuation_policy = match provider.continuation.as_ref() {
            Some(picked) => parse_continuation_policy(&picked.value).map_err(|error| {
                error.at(&format!(
                    "[providers.{id}].continuation in the {layer} config"
                ))
            })?,
            None => ModelContinuationPolicy::StatelessLocalReplay,
        };
        validate_protocol_axes(protocol, chat_reasoning_format, continuation_policy).map_err(
            |error| {
                error.at(&format!("[providers.{id}] in the {layer} config"))
            },
        )?;
        // Validate now so the merge at assembly time cannot fail. The SDK
        // adapter supplies its default User-Agent when none is configured.
        let mut request_headers = ModelRequestHeaders::new();
        for sourced in provider.headers.values() {
            request_headers
                .set(&sourced.value.name, &sourced.value.value)
                .map_err(|error| {
                    UsageError::new(error.to_string()).at(&format!(
                        "[providers.{id}.headers].{} in the {} config",
                        sourced.value.name,
                        sourced.layer.name()
                    ))
                })?;
        }
        let pairs: Vec<(String, String)> = provider
            .headers
            .iter()
            .map(|(canonical, sourced)| (canonical.clone(), sourced.value.value.clone()))
            .collect();
        let cache_policy = match provider.cache.as_ref() {
            Some(cache) => {
                let retention = match cache.retention.as_ref() {
                    Some(picked) => parse_cache_retention(&picked.value).map_err(|error| {
                        error.at(&format!(
                            "[providers.{id}.cache].retention in the {layer} config"
                        ))
                    })?,
                    None => CacheRetention::Short,
                };
                let hints = match cache.hints.as_ref() {
                    Some(picked) => parse_cache_hints(&picked.value).map_err(|error| {
                        error.at(&format!(
                            "[providers.{id}.cache].hints in the {layer} config"
                        ))
                    })?,
                    None => PromptCacheHints::default(),
                };
                ModelCachePolicy { retention, hints }
            }
            None => ModelCachePolicy::default(),
        };
        for (name, model) in &provider.models {
            let section = format!("[providers.{id}.models.{name}] in the {layer} config");
            let context_window = match model.context_window.as_ref() {
                Some(picked) => Some(positive_u64(&format!("[providers.{id}.models.{name}].context_window"), picked.value)?),
                None => None,
            };
            let max_output_tokens = match model.max_output_tokens.as_ref() {
                Some(picked) => Some(u32::try_from(picked.value).map_err(|_| {
                    UsageError::new(format!(
                        "[providers.{id}.models.{name}].max_output_tokens is out of range: {}",
                        picked.value
                    ))
                })?),
                None => None,
            };
            // Declared reasoning tiers, deduplicated in canonical effort
            // order. The default tier is the middle of the set.
            let mut reasoning_tiers: Vec<ReasoningEffort> = Vec::new();
            if let Some(picked) = model.reasoning.as_ref() {
                for value in &picked.value {
                    let origin = format!("[providers.{id}.models.{name}].reasoning entry '{value}' in the {layer} config");
                    let tier =
                        parse_reasoning_effort(value).map_err(|error| error.at(&origin))?;
                    validate_reasoning_effort(protocol, compat, chat_reasoning_format, tier)
                        .map_err(|error| error.at(&origin))?;
                    if !reasoning_tiers.contains(&tier) {
                        reasoning_tiers.push(tier);
                    }
                }
                reasoning_tiers.sort_by_key(|tier| effort_rank(*tier));
                if reasoning_tiers.is_empty() {
                    return Err(UsageError::new(format!(
                        "{section}: reasoning must not be empty; omit the key for a \
                         non-reasoning model"
                    )));
                }
            }
            let default_reasoning = reasoning_tiers
                .get(reasoning_tiers.len() / 2)
                .copied();
            let image_input = parse_modalities(
                &format!("[providers.{id}.models.{name}].input"),
                model.input.as_ref(),
                &["text", "image"],
                layer,
            )?
            .contains(&"image".to_owned());
            let output_modalities = parse_modalities(
                &format!("[providers.{id}.models.{name}].output"),
                model.output.as_ref(),
                &["text"],
                layer,
            )?;
            if output_modalities.is_empty() {
                return Err(UsageError::new(format!(
                    "{section}: output must declare at least one modality"
                )));
            }
            choices.push(ModelChoice {
                id: format!("{id}/{name}"),
                provider_id: id.clone(),
                model: name.clone(),
                endpoint: endpoint.value.clone(),
                protocol,
                credential: credential.clone(),
                compat,
                chat_reasoning_format,
                continuation_policy,
                context_window,
                request_headers: pairs.clone(),
                max_output_tokens,
                reasoning_tiers,
                default_reasoning,
                image_input,
                cache_policy,
            });
        }
    }
    choices.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(choices)
}

/// Resolves one deployment's credential: exactly one of `api_key` /
/// `api_key_env` may be set; with neither, the key is expected in
/// `PHILO_API_KEY`. The literal form is recorded masked so it can never
/// reach `/config`.
fn resolve_credential(
    section: &str,
    api_key: Option<&Sourced<FileSecret>>,
    api_key_env: Option<&Sourced<String>>,
    record: &mut dyn FnMut(&str, String, &str),
) -> Result<Credential, UsageError> {
    match (api_key, api_key_env) {
        (Some(_), Some(_)) => Err(UsageError::new(format!(
            "{section} sets both api_key and api_key_env; keep exactly one"
        ))),
        (Some(key), None) => {
            record("api_key", "<configured>".to_owned(), key.layer.name());
            Ok(Credential::Literal(key.value.0.clone()))
        }
        (None, Some(picked)) => {
            record("api_key_env", picked.value.clone(), picked.layer.name());
            Ok(Credential::EnvName(picked.value.clone()))
        }
        (None, None) => {
            record("api_key_env", API_KEY_ENV.to_owned(), "default");
            Ok(Credential::EnvName(API_KEY_ENV.to_owned()))
        }
    }
}

/// The `/config` label for a continuation policy.
fn continuation_label(policy: ModelContinuationPolicy) -> &'static str {
    match policy {
        ModelContinuationPolicy::StatelessLocalReplay => "stateless-local-replay",
        ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback => {
            "prefer-previous-response-id"
        }
    }
}

/// Validates `[aliases]`: every target must name a configured model choice.
fn resolve_aliases(
    file: &FileConfig,
    models: &[ModelChoice],
) -> Result<Vec<(String, String)>, UsageError> {
    let mut aliases = Vec::new();
    for (name, sourced) in &file.aliases {
        let target = sourced.value.trim();
        if !models.iter().any(|choice| choice.id == target) {
            return Err(UsageError::new(format!(
                "alias '{name}' targets '{target}', which is not a configured \
                 [providers] model"
            ))
            .at(&format!(
                "aliases.{name} in the {} config",
                sourced.layer.name()
            )));
        }
        aliases.push((name.clone(), target.to_owned()));
    }
    Ok(aliases)
}

/// Resolves a requested model id through aliases and the provider catalog.
/// Returns the deployment parameters to assemble with plus the wire-level
/// model name. The catalog is the only source of deployments, so an
/// unmatched name is an error rather than a silent fallback.
pub fn deployment_for(
    settings: &Settings,
    requested: &str,
) -> Result<(Deployment, String), UsageError> {
    let resolved = settings
        .aliases
        .iter()
        .find(|(name, _)| name == requested)
        .map_or(requested, |(_, target)| target.as_str());
    let choice = settings.models.iter().find(|choice| choice.id == resolved).ok_or_else(|| {
        UsageError::new(format!(
            "unknown model '{requested}': pick one of the configured [providers] models \
             (see /models)"
        ))
    })?;
    let deployment = deployment_from_choice(
        choice,
        settings.deployment.response_head_timeout,
        settings.deployment.stream_idle_timeout,
    );
    Ok((deployment, choice.model.clone()))
}

/// Builds a [`Deployment`] verbatim from a catalog entry plus the transport
/// deadlines carried by `[recovery]`.
fn deployment_from_choice(
    choice: &ModelChoice,
    response_head_timeout: Option<Duration>,
    stream_idle_timeout: Option<Duration>,
) -> Deployment {
    let mut request_headers = ModelRequestHeaders::new();
    for (name, value) in &choice.request_headers {
        // Validated during resolution; the rebuild cannot fail.
        request_headers
            .set(name, value)
            .expect("provider header was validated at resolution");
    }
    Deployment {
        provider: choice.provider_id.clone(),
        protocol: choice.protocol,
        // The stable composite identity: `deployment_for` resolves through
        // it again on every rebuild.
        model: choice.id.clone(),
        endpoint: choice.endpoint.clone(),
        credential: choice.credential.clone(),
        request_headers,
        compat: choice.compat,
        chat_reasoning_format: choice.chat_reasoning_format,
        continuation_policy: choice.continuation_policy,
        max_output_tokens: choice.max_output_tokens,
        default_reasoning: choice.default_reasoning,
        image_input: choice.image_input,
        cache_policy: choice.cache_policy,
        response_head_timeout,
        stream_idle_timeout,
    }
}

/// `/config` header rows for the active model's provider: configured headers
/// masked (User-Agent is display metadata), plus the SDK default UA when the
/// provider does not override it.
fn header_entries(choice: &ModelChoice) -> Vec<EffectiveSetting> {
    let mut entries: Vec<EffectiveSetting> = choice
        .request_headers
        .iter()
        .map(|(canonical, _)| EffectiveSetting {
            key: format!("header.{canonical}"),
            value: if canonical == "user-agent" {
                choice
                    .request_headers
                    .iter()
                    .find(|(name, _)| name == canonical)
                    .map(|(_, value)| value.clone())
                    .unwrap_or_default()
            } else {
                "<configured>".to_owned()
            },
            source: format!("providers.{}", choice.provider_id),
        })
        .collect();
    if !choice.request_headers.iter().any(|(name, _)| name == "user-agent") {
        entries.push(EffectiveSetting {
            key: "header.user-agent".to_owned(),
            value: DEFAULT_USER_AGENT.to_owned(),
            source: "default".to_owned(),
        });
    }
    entries
}

/// Validates a modality array against the supported vocabulary. Absent means
/// `["text"]`; duplicates collapse; an unknown member is a hard error.
fn parse_modalities(
    key: &str,
    modalities: Option<&Sourced<Vec<String>>>,
    allowed: &[&str],
    layer: &str,
) -> Result<Vec<String>, UsageError> {
    let Some(sourced) = modalities else {
        return Ok(vec!["text".to_owned()]);
    };
    let mut resolved = Vec::new();
    for value in &sourced.value {
        if !allowed.contains(&value.as_str()) {
            return Err(UsageError::new(format!(
                "{key} in the {layer} config lists unsupported modality '{value}': \
                 expected {}",
                allowed.join(" | ")
            )));
        }
        if !resolved.contains(value) {
            resolved.push(value.clone());
        }
    }
    Ok(resolved)
}

/// Canonical rank of a reasoning effort tier, from lightest to heaviest.
fn effort_rank(effort: ReasoningEffort) -> u8 {
    match effort {
        ReasoningEffort::Minimal => 0,
        ReasoningEffort::Low => 1,
        ReasoningEffort::Medium => 2,
        ReasoningEffort::High => 3,
        ReasoningEffort::Xhigh => 4,
        ReasoningEffort::Max => 5,
    }
}

/// `/reasoning` and `--reasoning-effort` validate against the active model's
/// declared tier set: an unlisted tier is refused even when the protocol
/// could encode it.
fn validate_reasoning_tiers(
    choice: &ModelChoice,
    effort: ReasoningEffort,
) -> Result<(), UsageError> {
    if choice.reasoning_tiers.contains(&effort) {
        return Ok(());
    }
    if choice.reasoning_tiers.is_empty() {
        return Err(UsageError::new(format!(
            "model '{}' does not support reasoning; remove --reasoning-effort or switch \
             to a model that declares reasoning tiers",
            choice.id
        )));
    }
    Err(UsageError::new(format!(
        "reasoning effort '{}' is not supported by model '{}': expected {}",
        effort_label(effort),
        choice.id,
        tiers_label(&choice.reasoning_tiers)
    )))
}

/// Comma-separated tier labels in canonical order.
fn tiers_label(tiers: &[ReasoningEffort]) -> String {
    let mut ranked: Vec<&ReasoningEffort> = tiers.iter().collect();
    ranked.sort_by_key(|tier| effort_rank(**tier));
    ranked
        .iter()
        .map(|tier| effort_label(**tier))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn positive_u64(key: &str, value: i64) -> Result<u64, UsageError> {
    u64::try_from(value).map_err(|_| UsageError::new(format!("{key} is out of range: {value}")))
}

fn positive_u32(key: &str, value: i64) -> Result<u32, UsageError> {
    u32::try_from(value).map_err(|_| UsageError::new(format!("{key} is out of range: {value}")))
}

fn threshold_f32(key: &str, value: f64) -> Result<f32, UsageError> {
    if !value.is_finite() || !(0.0 < value && value <= 1.0) {
        return Err(UsageError::new(format!(
            "{key} must be greater than 0 and at most 1"
        )));
    }
    Ok(value as f32)
}

pub(super) fn parse_protocol(value: &str) -> Result<ModelProtocol, UsageError> {
    match value {
        "openai-chat" => Ok(ModelProtocol::OpenAiChat),
        "openai-responses" => Ok(ModelProtocol::OpenAiResponses),
        "openai-chat-compatible" => Err(UsageError::new(
            "protocol 'openai-chat-compatible' is no longer accepted; use protocol=openai-chat \
             and compat=compatible. For the old compatible-v2 behavior also set \
             reasoning_format=none",
        )),
        "openai-chat-compatible-reasoning-effort" => Err(UsageError::new(
            "protocol 'openai-chat-compatible-reasoning-effort' is no longer accepted; use \
             protocol=openai-chat and compat=compatible with reasoning_format=effort-only",
        )),
        "openai-chat-reasoning-content" => Err(UsageError::new(
            "protocol 'openai-chat-reasoning-content' is no longer accepted; use \
             protocol=openai-chat and compat=compatible with reasoning_format=content-only",
        )),
        "anthropic-messages" => Err(UsageError::new(
            "protocol 'anthropic-messages' is not supported in this version",
        )),
        other => Err(UsageError::new(format!(
            "unknown protocol '{other}': expected openai-chat | openai-responses"
        ))),
    }
}

pub(super) fn parse_compat(value: &str) -> Result<ModelCompat, UsageError> {
    match value {
        "official" => Ok(ModelCompat::Official),
        "compatible" => Ok(ModelCompat::Compatible),
        other => Err(UsageError::new(format!(
            "unknown compat '{other}': expected official | compatible"
        ))),
    }
}

pub(super) fn parse_reasoning_format(value: &str) -> Result<ChatReasoningFormat, UsageError> {
    match value {
        "none" => Ok(ChatReasoningFormat::None),
        "effort-only" => Ok(ChatReasoningFormat::EffortOnly),
        "content-only" => Ok(ChatReasoningFormat::ContentOnly),
        "effort-and-content" => Ok(ChatReasoningFormat::EffortAndContent),
        other => Err(UsageError::new(format!(
            "unknown reasoning_format '{other}': expected none | effort-only | content-only | \
             effort-and-content"
        ))),
    }
}

pub(super) fn parse_continuation_policy(
    value: &str,
) -> Result<ModelContinuationPolicy, UsageError> {
    match value {
        "stateless-local-replay" => Ok(ModelContinuationPolicy::StatelessLocalReplay),
        "prefer-previous-response-id" => {
            Ok(ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback)
        }
        other => Err(UsageError::new(format!(
            "unknown continuation policy '{other}': expected stateless-local-replay | prefer-previous-response-id"
        ))),
    }
}

fn validate_protocol_axes(
    protocol: ModelProtocol,
    chat_reasoning_format: Option<ChatReasoningFormat>,
    policy: ModelContinuationPolicy,
) -> Result<(), UsageError> {
    if chat_reasoning_format.is_some() && protocol != ModelProtocol::OpenAiChat {
        return Err(UsageError::new(
            "reasoning_format is only valid for the OpenAI Chat protocol",
        ));
    }
    if policy == ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback
        && protocol != ModelProtocol::OpenAiResponses
    {
        return Err(UsageError::new(
            "prefer-previous-response-id requires the OpenAI Responses protocol",
        ));
    }
    Ok(())
}

pub(super) fn parse_reasoning_effort(value: &str) -> Result<ReasoningEffort, UsageError> {
    match value {
        "minimal" => Ok(ReasoningEffort::Minimal),
        "low" => Ok(ReasoningEffort::Low),
        "medium" => Ok(ReasoningEffort::Medium),
        "high" => Ok(ReasoningEffort::High),
        "xhigh" => Ok(ReasoningEffort::Xhigh),
        "max" => Ok(ReasoningEffort::Max),
        other => Err(UsageError::new(format!(
            "invalid reasoning effort '{other}': expected minimal | low | medium | high | xhigh | max"
        ))),
    }
}

/// The canonical lowercase label for a resolved reasoning effort.
pub(crate) fn effort_label(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Minimal => "minimal",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Xhigh => "xhigh",
        ReasoningEffort::Max => "max",
    }
}

pub(super) fn parse_cache_retention(value: &str) -> Result<CacheRetention, UsageError> {
    match value {
        "none" => Ok(CacheRetention::None),
        "short" => Ok(CacheRetention::Short),
        "long" => Ok(CacheRetention::Long),
        other => Err(UsageError::new(format!(
            "invalid cache retention '{other}': expected none | short | long"
        ))),
    }
}

pub(super) fn parse_cache_hints(values: &[String]) -> Result<PromptCacheHints, UsageError> {
    let mut hints = PromptCacheHints::default();
    for value in values {
        match value.as_str() {
            "instructions" => hints.instructions = true,
            "tools" => hints.tools = true,
            "history" => hints.history = true,
            other => {
                return Err(UsageError::new(format!(
                    "invalid cache hint '{other}': expected instructions | tools | history"
                )));
            }
        }
    }
    Ok(hints)
}

pub(super) fn validate_reasoning_effort(
    protocol: ModelProtocol,
    compat: ModelCompat,
    chat_reasoning_format: Option<ChatReasoningFormat>,
    effort: ReasoningEffort,
) -> Result<(), UsageError> {
    if protocol.supports_reasoning_effort(compat, chat_reasoning_format, effort) {
        return Ok(());
    }
    Err(UsageError::new(format!(
        "reasoning effort is unsupported by protocol '{}' with compat '{}'; remove the \
         reasoning setting or choose a protocol that supports it",
        protocol.as_str(),
        compat.as_str()
    )))
}

pub(super) fn parse_verbosity(value: &str) -> Result<Verbosity, UsageError> {
    match value {
        "default" => Ok(Verbosity::Default),
        "verbose" => Ok(Verbosity::Verbose),
        "quiet" => Ok(Verbosity::Quiet),
        other => Err(UsageError::new(format!(
            "invalid [ui].verbosity '{other}': expected default | verbose | quiet"
        ))),
    }
}

/// Resolves the session data directory and names the layer it came from:
/// flag > PHILO_DATA_DIR > project > global > ~/.philo/sessions.
pub(super) fn resolve_data_dir(
    flag: Option<PathBuf>,
    file: &FileConfig,
) -> Result<(PathBuf, &'static str), UsageError> {
    if let Some(dir) = flag {
        return Ok((dir, "flag"));
    }
    if let Ok(dir) = std::env::var("PHILO_DATA_DIR") {
        return Ok((PathBuf::from(dir), "env"));
    }
    if let Some(sourced) = &file.data_dir {
        return Ok((PathBuf::from(&sourced.value), sourced.layer.name()));
    }
    dirs::home_dir()
        .map(|home| (home.join(".philo").join("sessions"), "default"))
        .ok_or_else(|| {
            UsageError::new(
                "cannot determine the home directory: pass --data-dir, set PHILO_DATA_DIR, or \
                 set [deployment].data_dir",
            )
        })
}
