//! Effective run settings: flag > environment > project file > global file
//! > built-in/profile default.

use std::path::PathBuf;
use std::time::Duration;

use philo_agent_runtime::ReasoningEffort;
use philo_model::ModelProtocol;

use super::file::{FileConfig, Sourced};
use crate::args::Cli;
use crate::error::UsageError;

const API_KEY_ENV: &str = "PHILO_API_KEY";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verbosity {
    Default,
    Verbose,
    Quiet,
}

/// Model deployment configuration owned by the composition root.
#[derive(Clone, Debug)]
pub struct Deployment {
    pub provider: String,
    pub protocol: ModelProtocol,
    pub model: String,
    pub endpoint: String,
    /// Name of the environment variable carrying the API key — never the key.
    pub api_key_env: String,
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
pub struct Settings {
    pub deployment: Deployment,
    pub data_dir: PathBuf,
    pub context_window: Option<u64>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub max_tool_rounds: Option<u32>,
    pub operation_timeout: Option<Duration>,
    pub shell_timeout_secs: Option<u64>,
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

    let model = pick_string(
        cli.model.as_deref(),
        Some("PHILO_MODEL"),
        file.model.as_ref(),
    )
    .ok_or_else(|| {
        UsageError::new(
            "no model configured: pass --model, set PHILO_MODEL, or set \
             [deployment].model in config.toml",
        )
    })?;
    record("model", model.value.clone(), model.source);

    let endpoint =
        pick_string(None, Some("PHILO_ENDPOINT"), file.endpoint.as_ref()).ok_or_else(|| {
            UsageError::new(
                "no endpoint configured: set PHILO_ENDPOINT or [deployment].endpoint in \
                 config.toml to the full request URL",
            )
        })?;
    record("endpoint", endpoint.value.clone(), endpoint.source);

    let protocol = match pick_string(None, Some("PHILO_PROTOCOL"), file.protocol.as_ref()) {
        Some(picked) => {
            record("protocol", picked.value.clone(), picked.source);
            parse_protocol(&picked.value).map_err(|error| {
                error.at(&origin(
                    picked.source,
                    None,
                    Some("PHILO_PROTOCOL"),
                    "[deployment].protocol",
                ))
            })?
        }
        None => {
            record("protocol", "openai-chat-compatible".to_owned(), "default");
            ModelProtocol::OpenAiChatCompatible
        }
    };

    let provider = match pick_string(None, Some("PHILO_PROVIDER"), file.provider.as_ref()) {
        Some(picked) => {
            record("provider", picked.value.clone(), picked.source);
            picked.value
        }
        None => {
            record("provider", "philo-cli".to_owned(), "default");
            "philo-cli".to_owned()
        }
    };

    let api_key_env = match pick_string(None, None, file.api_key_env.as_ref()) {
        Some(picked) => {
            record("api_key_env", picked.value.clone(), picked.source);
            picked.value
        }
        None => {
            record("api_key_env", API_KEY_ENV.to_owned(), "default");
            API_KEY_ENV.to_owned()
        }
    };

    let (data_dir, data_dir_source) = resolve_data_dir(cli.data_dir.clone(), file)?;
    record("data_dir", data_dir.display().to_string(), data_dir_source);

    let context_window = match from_file(file.context_window.as_ref()) {
        Some(picked) => {
            record("context_window", picked.value.to_string(), picked.source);
            Some(positive_u64("[deployment].context_window", picked.value)?)
        }
        None => None,
    };

    let reasoning_effort = match pick_string(
        cli.reasoning_effort.as_deref(),
        None,
        file.reasoning_effort.as_ref(),
    ) {
        Some(picked) => {
            record("reasoning_effort", picked.value.clone(), picked.source);
            Some(parse_reasoning_effort(&picked.value).map_err(|error| {
                error.at(&origin(
                    picked.source,
                    Some("--reasoning-effort"),
                    None,
                    "[defaults].reasoning_effort",
                ))
            })?)
        }
        None => None,
    };

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

    Ok(Settings {
        deployment: Deployment {
            provider,
            protocol,
            model: model.value,
            endpoint: endpoint.value,
            api_key_env,
        },
        data_dir,
        context_window,
        reasoning_effort,
        max_tool_rounds,
        operation_timeout,
        shell_timeout_secs,
        verbosity,
        show_reasoning,
        entries,
    })
}

fn positive_u64(key: &str, value: i64) -> Result<u64, UsageError> {
    u64::try_from(value).map_err(|_| UsageError::new(format!("{key} is out of range: {value}")))
}

pub(super) fn parse_protocol(value: &str) -> Result<ModelProtocol, UsageError> {
    match value {
        "anthropic-messages" => Ok(ModelProtocol::AnthropicMessages),
        "openai-chat" => Ok(ModelProtocol::OpenAiChat),
        "openai-chat-compatible" => Ok(ModelProtocol::OpenAiChatCompatible),
        "openai-chat-reasoning-content" => Ok(ModelProtocol::OpenAiChatReasoningContent),
        "openai-responses" => Ok(ModelProtocol::OpenAiResponses),
        other => Err(UsageError::new(format!(
            "unknown protocol '{other}': expected anthropic-messages | openai-chat | \
             openai-chat-compatible | openai-chat-reasoning-content | openai-responses"
        ))),
    }
}

pub(super) fn parse_reasoning_effort(value: &str) -> Result<ReasoningEffort, UsageError> {
    match value {
        "minimal" => Ok(ReasoningEffort::Minimal),
        "low" => Ok(ReasoningEffort::Low),
        "medium" => Ok(ReasoningEffort::Medium),
        "high" => Ok(ReasoningEffort::High),
        "very-high" => Ok(ReasoningEffort::VeryHigh),
        "maximum" => Ok(ReasoningEffort::Maximum),
        other => Err(UsageError::new(format!(
            "invalid reasoning effort '{other}': expected minimal | low | medium | high | \
             very-high | maximum"
        ))),
    }
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
