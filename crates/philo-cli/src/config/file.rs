//! Two-layer TOML loading and key-level merging.
//!
//! `~/.philo/config.toml` supplies the global layer and
//! `<workspace>/.philo/config.toml` supplies the project layer. Every value
//! retains its source so resolution diagnostics and `/config` stay honest.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::error::UsageError;

const CONFIG_HOME_ENV: &str = "PHILO_CONFIG_HOME";
const CONFIG_FILE: &str = "config.toml";
const SECRET_KEYS: [&str; 6] = ["api_key", "apikey", "key", "token", "secret", "password"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Layer {
    Global,
    Project,
}

impl Layer {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Sourced<T> {
    pub(super) value: T,
    pub(super) layer: Layer,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct FileHeader {
    pub(super) name: String,
    pub(super) value: String,
}

impl fmt::Debug for FileHeader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileHeader")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Default)]
pub(super) struct FileConfig {
    // [deployment]
    pub(super) model: Option<Sourced<String>>,
    pub(super) endpoint: Option<Sourced<String>>,
    pub(super) protocol: Option<Sourced<String>>,
    pub(super) provider: Option<Sourced<String>>,
    pub(super) api_key_env: Option<Sourced<String>>,
    pub(super) data_dir: Option<Sourced<String>>,
    pub(super) context_window: Option<Sourced<i64>>,
    pub(super) continuation: Option<Sourced<String>>,
    pub(super) compat: Option<Sourced<String>>,
    pub(super) reasoning_format: Option<Sourced<String>>,
    /// Canonical lowercase header name -> highest-authority configured value.
    pub(super) headers: BTreeMap<String, Sourced<FileHeader>>,
    // [compaction]
    pub(super) compaction_context_budget: Option<Sourced<i64>>,
    pub(super) compaction_auto_threshold: Option<Sourced<f64>>,
    pub(super) compaction_keep_recent_turns: Option<Sourced<i64>>,
    pub(super) compaction_estimate_bytes_per_token: Option<Sourced<i64>>,
    // [defaults]
    pub(super) reasoning_effort: Option<Sourced<String>>,
    pub(super) max_tool_rounds: Option<Sourced<i64>>,
    pub(super) max_parallel_tool_calls: Option<Sourced<i64>>,
    pub(super) operation_timeout_secs: Option<Sourced<i64>>,
    // [tools]
    pub(super) shell_timeout_secs: Option<Sourced<i64>>,
    // [ui]
    pub(super) verbosity: Option<Sourced<String>>,
    pub(super) show_reasoning: Option<Sourced<bool>>,
    pub(super) screen: Option<Sourced<String>>,
    /// Unknown sections and keys are retained as forward-compatible warnings.
    pub(super) warnings: Vec<String>,
}

fn global_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(CONFIG_HOME_ENV) {
        return Some(PathBuf::from(dir).join(CONFIG_FILE));
    }
    dirs::home_dir().map(|home| home.join(".philo").join(CONFIG_FILE))
}

fn project_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".philo").join(CONFIG_FILE)
}

pub(super) fn load() -> Result<FileConfig, UsageError> {
    let workspace_root = std::env::current_dir().map_err(|error| {
        UsageError::new(format!("cannot resolve the working directory: {error}"))
    })?;
    load_layers(
        global_path().as_deref(),
        Some(&project_path(&workspace_root)),
    )
}

/// Loads the given layers in priority order (global first, project last).
pub(super) fn load_layers(
    global: Option<&Path>,
    project: Option<&Path>,
) -> Result<FileConfig, UsageError> {
    let mut config = FileConfig::default();
    for (layer, path) in [(Layer::Global, global), (Layer::Project, project)] {
        let Some(path) = path else { continue };
        if !path.exists() {
            continue;
        }
        let text = std::fs::read_to_string(path).map_err(|error| {
            UsageError::new(format!(
                "cannot read the {} config '{}': {}",
                layer.name(),
                path.display(),
                error.kind()
            ))
        })?;
        // Deliberately suppress parser diagnostics: source text may contain a
        // misplaced secret and must never be echoed.
        let table: toml::Table = text.parse().map_err(|_error| {
            UsageError::new(format!(
                "invalid TOML in the {} config '{}'; fix the file syntax",
                layer.name(),
                path.display()
            ))
        })?;
        apply(&mut config, layer, path, &table)?;
    }
    Ok(config)
}

fn apply(
    config: &mut FileConfig,
    layer: Layer,
    path: &Path,
    table: &toml::Table,
) -> Result<(), UsageError> {
    for (section, value) in table {
        let Some(entries) = value.as_table() else {
            config.warnings.push(format!(
                "{}: '{section}' is not a section; ignored",
                path.display()
            ));
            continue;
        };
        // A secret is refused wherever it appears, known section or not.
        for key in entries.keys() {
            if SECRET_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
                return Err(UsageError::new(format!(
                    "{}: [{section}].{key} would store a secret in a file; philo reads \
                     credentials from the environment only - set [deployment].api_key_env to \
                     the variable name instead",
                    path.display()
                )));
            }
        }
        if !matches!(
            section.as_str(),
            "deployment" | "defaults" | "tools" | "ui" | "compaction"
        ) {
            config.warnings.push(format!(
                "{}: unknown section [{section}]; ignored",
                path.display()
            ));
            continue;
        }
        for (key, value) in entries {
            let reader = Reader {
                path,
                layer,
                section,
                key,
                value,
            };
            match (section.as_str(), key.as_str()) {
                ("deployment", "model") => config.model = Some(reader.string()?),
                ("deployment", "endpoint") => config.endpoint = Some(reader.string()?),
                ("deployment", "protocol") => config.protocol = Some(reader.string()?),
                ("deployment", "provider") => config.provider = Some(reader.string()?),
                ("deployment", "api_key_env") => config.api_key_env = Some(reader.env_name()?),
                ("deployment", "data_dir") => config.data_dir = Some(reader.string()?),
                ("deployment", "context_window") => {
                    config.context_window = Some(reader.integer()?);
                }
                ("deployment", "continuation") => {
                    config.continuation = Some(reader.string()?);
                }
                ("deployment", "compat") => config.compat = Some(reader.string()?),
                ("deployment", "reasoning_format") => {
                    config.reasoning_format = Some(reader.string()?);
                }
                ("deployment", "response_continuation_support") => {
                    return Err(UsageError::new(format!(
                        "{}: [deployment].response_continuation_support has been removed; \
                         for OpenAI Responses continuation set [deployment].compat and \
                         continuation = \"prefer-previous-response-id\"",
                        path.display()
                    )));
                }
                ("deployment", "headers") => {
                    apply_headers(&mut config.headers, layer, path, value)?;
                }
                ("compaction", "context_budget") => {
                    config.compaction_context_budget = Some(reader.integer()?);
                }
                ("compaction", "auto_threshold") => {
                    config.compaction_auto_threshold = Some(reader.number()?);
                }
                ("compaction", "keep_recent_turns") => {
                    config.compaction_keep_recent_turns = Some(reader.integer()?);
                }
                ("compaction", "estimate_bytes_per_token") => {
                    config.compaction_estimate_bytes_per_token = Some(reader.integer()?);
                }
                ("defaults", "reasoning_effort") => {
                    config.reasoning_effort = Some(reader.string()?);
                }
                ("defaults", "max_tool_rounds") => config.max_tool_rounds = Some(reader.integer()?),
                ("defaults", "max_parallel_tool_calls") => {
                    config.max_parallel_tool_calls = Some(reader.integer()?);
                }
                ("defaults", "operation_timeout_secs") => {
                    config.operation_timeout_secs = Some(reader.integer()?);
                }
                ("tools", "shell_timeout_secs") => {
                    config.shell_timeout_secs = Some(reader.integer()?);
                }
                ("ui", "verbosity") => config.verbosity = Some(reader.string()?),
                ("ui", "show_reasoning") => config.show_reasoning = Some(reader.boolean()?),
                ("ui", "screen") => config.screen = Some(reader.string()?),
                _ => config.warnings.push(format!(
                    "{}: unknown key [{section}].{key}; ignored",
                    path.display()
                )),
            }
        }
    }
    Ok(())
}

fn apply_headers(
    configured: &mut BTreeMap<String, Sourced<FileHeader>>,
    layer: Layer,
    path: &Path,
    value: &toml::Value,
) -> Result<(), UsageError> {
    let headers = value.as_table().ok_or_else(|| {
        UsageError::new(format!(
            "{}: [deployment].headers must be a table, found {}",
            path.display(),
            value.type_str()
        ))
    })?;
    for (name, value) in headers {
        let canonical = name.to_ascii_lowercase();
        if SECRET_KEYS.contains(&canonical.as_str()) {
            return Err(UsageError::new(format!(
                "{}: [deployment.headers].{name} would store a secret in a file; philo reads credentials from the environment only - set [deployment].api_key_env to the variable name instead",
                path.display()
            )));
        }
        let reader = Reader {
            path,
            layer,
            section: "deployment.headers",
            key: name,
            value,
        };
        let value = reader.string()?;
        configured.insert(
            canonical,
            Sourced {
                value: FileHeader {
                    name: name.clone(),
                    value: value.value,
                },
                layer,
            },
        );
    }
    Ok(())
}

/// One key being read, with everything an error message needs.
struct Reader<'a> {
    path: &'a Path,
    layer: Layer,
    section: &'a str,
    key: &'a str,
    value: &'a toml::Value,
}

impl Reader<'_> {
    fn wrong_type(&self, expected: &str) -> UsageError {
        UsageError::new(format!(
            "{}: [{}].{} must be {expected}, found {}",
            self.path.display(),
            self.section,
            self.key,
            self.value.type_str()
        ))
    }

    fn string(&self) -> Result<Sourced<String>, UsageError> {
        let value = self
            .value
            .as_str()
            .ok_or_else(|| self.wrong_type("a string"))?;
        if value.trim().is_empty() {
            return Err(UsageError::new(format!(
                "{}: [{}].{} must not be empty",
                self.path.display(),
                self.section,
                self.key
            )));
        }
        Ok(Sourced {
            value: value.to_owned(),
            layer: self.layer,
        })
    }

    /// Credential configuration stores an environment variable name, not a
    /// value. The portable identifier grammar also prevents a pasted key
    /// from later reaching `/config`.
    fn env_name(&self) -> Result<Sourced<String>, UsageError> {
        let sourced = self.string()?;
        let mut chars = sourced.value.chars();
        let valid_first = chars
            .next()
            .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic());
        let valid_rest = chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric());
        if !valid_first || !valid_rest {
            return Err(UsageError::new(format!(
                "{}: [{}].{} must be an environment variable name; the value was not loaded",
                self.path.display(),
                self.section,
                self.key
            )));
        }
        Ok(sourced)
    }

    fn integer(&self) -> Result<Sourced<i64>, UsageError> {
        let value = self
            .value
            .as_integer()
            .ok_or_else(|| self.wrong_type("an integer"))?;
        if value <= 0 {
            return Err(UsageError::new(format!(
                "{}: [{}].{} must be a positive integer, found {value}",
                self.path.display(),
                self.section,
                self.key
            )));
        }
        Ok(Sourced {
            value,
            layer: self.layer,
        })
    }

    fn number(&self) -> Result<Sourced<f64>, UsageError> {
        let value = self
            .value
            .as_float()
            .or_else(|| self.value.as_integer().map(|value| value as f64))
            .ok_or_else(|| self.wrong_type("a number"))?;
        Ok(Sourced {
            value,
            layer: self.layer,
        })
    }

    fn boolean(&self) -> Result<Sourced<bool>, UsageError> {
        let value = self
            .value
            .as_bool()
            .ok_or_else(|| self.wrong_type("a boolean"))?;
        Ok(Sourced {
            value,
            layer: self.layer,
        })
    }
}
