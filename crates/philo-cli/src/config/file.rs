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

/// One `[providers.<id>.models.<model>]` entry. Model entries are the final
/// source of truth: every capability below lives here or nowhere.
#[derive(Debug, Default)]
pub(super) struct ModelFile {
    /// Context window of this model in tokens; feeds the compaction budget.
    pub(super) context_window: Option<Sourced<i64>>,
    /// Output cap declared by this model.
    pub(super) max_output_tokens: Option<Sourced<i64>>,
    /// Reasoning tiers this model supports. Absent means the model does not
    /// reason; the effective default tier is the middle of the set.
    pub(super) reasoning: Option<Sourced<Vec<String>>>,
    /// Input modalities; absent means text-only.
    pub(super) input: Option<Sourced<Vec<String>>>,
    /// Output modalities; only `text` is supported by the runtime.
    pub(super) output: Option<Sourced<Vec<String>>>,
}

/// One `[providers.<id>.cache]` section: how cache identity and breakpoints
/// are encoded for this provider's requests.
#[derive(Debug, Default)]
pub(super) struct CacheFile {
    pub(super) retention: Option<Sourced<String>>,
    pub(super) hints: Option<Sourced<Vec<String>>>,
}

/// One `[providers.<id>]` section: an endpoint plus the models it serves.
#[derive(Debug, Default)]
pub(super) struct ProviderFile {
    pub(super) endpoint: Option<Sourced<String>>,
    pub(super) protocol: Option<Sourced<String>>,
    pub(super) api_key_env: Option<Sourced<String>>,
    pub(super) api_key: Option<Sourced<FileSecret>>,
    pub(super) compat: Option<Sourced<String>>,
    pub(super) reasoning_format: Option<Sourced<String>>,
    /// Continuation policy for the OpenAI Responses protocol.
    pub(super) continuation: Option<Sourced<String>>,
    /// Canonical lowercase header name -> configured value; sent with every
    /// request to this provider.
    pub(super) headers: BTreeMap<String, Sourced<FileHeader>>,
    /// Model entries served by this provider, keyed by wire-level name.
    pub(super) models: BTreeMap<String, ModelFile>,
    /// Cache identity policy shared by every request to this provider.
    pub(super) cache: Option<CacheFile>,
}

/// A literal credential read from a config file. The value renders as
/// `<redacted>` in diagnostics and `/config`; only the assembler sees it.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct FileSecret(pub(super) String);

impl fmt::Debug for FileSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("FileSecret(<redacted>)")
    }
}

#[derive(Debug, Default)]
pub(super) struct FileConfig {
    // top-level keys
    pub(super) data_dir: Option<Sourced<String>>,
    /// Named provider deployments (`[providers.<id>]`), keyed by id.
    pub(super) providers: BTreeMap<String, Sourced<ProviderFile>>,
    /// Short names for composite model ids (`[aliases]`).
    pub(super) aliases: BTreeMap<String, Sourced<String>>,
    // [compaction]
    pub(super) compaction_context_budget: Option<Sourced<i64>>,
    pub(super) compaction_auto_threshold: Option<Sourced<f64>>,
    pub(super) compaction_keep_recent_turns: Option<Sourced<i64>>,
    pub(super) compaction_estimate_bytes_per_token: Option<Sourced<i64>>,
    // [defaults]
    pub(super) max_tool_rounds: Option<Sourced<i64>>,
    pub(super) max_parallel_tool_calls: Option<Sourced<i64>>,
    pub(super) operation_timeout_secs: Option<Sourced<i64>>,
    // [tools]
    pub(super) shell_timeout_secs: Option<Sourced<i64>>,
    // [recovery]
    pub(super) recovery_enabled: Option<Sourced<bool>>,
    pub(super) recovery_max_retries: Option<Sourced<i64>>,
    pub(super) recovery_backoff_base_ms: Option<Sourced<i64>>,
    pub(super) recovery_backoff_max_ms: Option<Sourced<i64>>,
    pub(super) recovery_response_head_timeout_secs: Option<Sourced<i64>>,
    pub(super) recovery_stream_idle_timeout_secs: Option<Sourced<i64>>,
    // [ui]
    pub(super) verbosity: Option<Sourced<String>>,
    pub(super) show_reasoning: Option<Sourced<bool>>,
    pub(super) screen: Option<Sourced<String>>,
    /// Explicit terminal background override (`#RRGGBB`) injected into the
    /// TUI palette; empty means the TUI uses its stable fallback surfaces.
    pub(super) terminal_bg: Option<Sourced<String>>,
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
        if layer == Layer::Project {
            warn_project_layer_secrets(&mut config, path);
        }
    }
    Ok(config)
}

/// Warns when a project-layer file carries a literal credential: project
/// configs are likely committed, so the key belongs in the environment or
/// the user's global config.
fn warn_project_layer_secrets(config: &mut FileConfig, path: &Path) {
    for (id, provider) in &config.providers {
        if provider
            .value
            .api_key
            .as_ref()
            .is_some_and(|key| key.layer == Layer::Project)
        {
            config.warnings.push(format!(
                "{}: [providers.{id}].api_key stores a literal credential in the project \
                 layer; prefer api_key_env, or move the key to the global config",
                path.display()
            ));
        }
    }
}

fn apply(
    config: &mut FileConfig,
    layer: Layer,
    path: &Path,
    table: &toml::Table,
) -> Result<(), UsageError> {
    for (section, value) in table {
        let Some(entries) = value.as_table() else {
            // Top-level scalar keys live outside any section.
            if section == "data_dir" {
                let reader = Reader {
                    path,
                    layer,
                    section: "(root)",
                    key: "data_dir",
                    value,
                };
                config.data_dir = Some(reader.string()?);
            } else {
                config.warnings.push(format!(
                    "{}: '{section}' is not a section; ignored",
                    path.display()
                ));
            }
            continue;
        };
        if section == "deployment" {
            return Err(UsageError::new(format!(
                "{}: [deployment] has been removed; every model now comes from the \
                 [providers] catalog - move endpoint and credentials into a \
                 [providers.<id>] section and list the models under \
                 [providers.<id>.models]",
                path.display()
            )));
        }
        // A secret is refused wherever it appears, known section or not —
        // except the sanctioned literal credential key `api_key`, which is
        // only accepted inside [providers.<id>].
        for key in entries.keys() {
            if key == "api_key" && section == "providers" {
                continue;
            }
            if SECRET_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
                return Err(UsageError::new(format!(
                    "{}: [{section}].{key} would store a secret in a file; philo reads \
                     credentials from the environment only - set api_key_env to the \
                     variable name instead",
                    path.display()
                )));
            }
        }
        if !matches!(
            section.as_str(),
            "defaults" | "tools" | "ui" | "compaction" | "recovery" | "providers" | "aliases"
        ) {
            config.warnings.push(format!(
                "{}: unknown section [{section}]; ignored",
                path.display()
            ));
            continue;
        }
        if section == "providers" {
            apply_providers(config, layer, path, entries)?;
            continue;
        }
        if section == "aliases" {
            for (name, value) in entries {
                let reader = Reader {
                    path,
                    layer,
                    section: "aliases",
                    key: name,
                    value,
                };
                let target = reader.string()?;
                validate_alias_name(path, layer, name)?;
                config.aliases.insert(
                    name.to_owned(),
                    Sourced {
                        value: target.value,
                        layer,
                    },
                );
            }
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
                    return Err(UsageError::new(format!(
                        "{}: [defaults].reasoning_effort has been removed; reasoning tiers \
                         are a model capability - declare them on the model with \
                         [providers.<id>.models.<model>].reasoning",
                        path.display()
                    )));
                }
                ("defaults", "max_tool_rounds") => config.max_tool_rounds = Some(reader.integer()?),
                ("defaults", "max_output_tokens") => {
                    return Err(UsageError::new(format!(
                        "{}: [defaults].max_output_tokens has been removed; the output cap \
                         is a model capability - declare it on the model with \
                         [providers.<id>.models.<model>].max_output_tokens",
                        path.display()
                    )));
                }
                ("defaults", "max_parallel_tool_calls") => {
                    config.max_parallel_tool_calls = Some(reader.integer()?);
                }
                ("defaults", "operation_timeout_secs") => {
                    config.operation_timeout_secs = Some(reader.integer()?);
                }
                ("tools", "shell_timeout_secs") => {
                    config.shell_timeout_secs = Some(reader.integer()?);
                }
                ("recovery", "enabled") => config.recovery_enabled = Some(reader.boolean()?),
                ("recovery", "max_retries") => {
                    config.recovery_max_retries = Some(reader.non_negative_integer()?);
                }
                ("recovery", "backoff_base_ms") => {
                    config.recovery_backoff_base_ms = Some(reader.integer()?);
                }
                ("recovery", "backoff_max_ms") => {
                    config.recovery_backoff_max_ms = Some(reader.integer()?);
                }
                ("recovery", "response_head_timeout_secs") => {
                    config.recovery_response_head_timeout_secs =
                        Some(reader.non_negative_integer()?);
                }
                ("recovery", "stream_idle_timeout_secs") => {
                    config.recovery_stream_idle_timeout_secs = Some(reader.non_negative_integer()?);
                }
                ("ui", "verbosity") => config.verbosity = Some(reader.string()?),
                ("ui", "show_reasoning") => config.show_reasoning = Some(reader.boolean()?),
                ("ui", "screen") => config.screen = Some(reader.string()?),
                ("ui", "terminal_bg") => config.terminal_bg = Some(reader.string()?),
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
    section: &str,
    value: &toml::Value,
) -> Result<(), UsageError> {
    let headers = value.as_table().ok_or_else(|| {
        UsageError::new(format!(
            "{}: {section} must be a table, found {}",
            path.display(),
            value.type_str()
        ))
    })?;
    for (name, value) in headers {
        let canonical = name.to_ascii_lowercase();
        if SECRET_KEYS.contains(&canonical.as_str()) {
            return Err(UsageError::new(format!(
                "{}: {section}.{name} would store a secret in a file; philo reads \
                 credentials from the environment only - set api_key_env or api_key instead",
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

/// Parses `[providers]` sub-tables. A later layer re-defining a provider id
/// replaces the earlier definition wholesale, mirroring key-level authority.
fn apply_providers(
    config: &mut FileConfig,
    layer: Layer,
    path: &Path,
    entries: &toml::value::Table,
) -> Result<(), UsageError> {
    for (id, value) in entries {
        let Some(table) = value.as_table() else {
            config.warnings.push(format!(
                "{}: [providers.{id}] is not a section; ignored",
                path.display()
            ));
            continue;
        };
        if id.trim().is_empty() || !valid_provider_id(id) {
            return Err(UsageError::new(format!(
                "{}: [providers.{id}] uses an invalid provider id; use letters, digits, \
                 '-' or '_'",
                path.display()
            )));
        }
        for key in table.keys() {
            if key == "api_key" {
                continue;
            }
            if SECRET_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
                return Err(UsageError::new(format!(
                    "{}: [providers.{id}].{key} would store a secret in a file; philo reads \
                     credentials from the environment only - set api_key_env to the variable \
                     name instead",
                    path.display()
                )));
            }
        }
        let mut provider = ProviderFile::default();
        for (key, value) in table {
            let reader = Reader {
                path,
                layer,
                section: "providers",
                key: &format!("{id}.{key}"),
                value,
            };
            match key.as_str() {
                "endpoint" => provider.endpoint = Some(reader.string()?),
                "protocol" => provider.protocol = Some(reader.string()?),
                "api_key_env" => provider.api_key_env = Some(reader.env_name()?),
                "api_key" => provider.api_key = Some(reader.secret()?),
                "compat" => provider.compat = Some(reader.string()?),
                "reasoning_format" => provider.reasoning_format = Some(reader.string()?),
                "continuation" => provider.continuation = Some(reader.string()?),
                "headers" => {
                    apply_headers(
                        &mut provider.headers,
                        layer,
                        path,
                        &format!("[providers.{id}.headers]"),
                        value,
                    )?;
                }
                "models" => {
                    let table = value.as_table().ok_or_else(|| {
                        UsageError::new(format!(
                            "{}: [providers.{id}].models must be a table of models, \
                             one [providers.{id}.models.<name>] section per model",
                            path.display()
                        ))
                    })?;
                    for (name, entry) in table {
                        if name.trim().is_empty() {
                            return Err(UsageError::new(format!(
                                "{}: [providers.{id}].models contains an empty model name",
                                path.display()
                            )));
                        }
                        if SECRET_KEYS.contains(&name.to_ascii_lowercase().as_str()) {
                            return Err(UsageError::new(format!(
                                "{}: [providers.{id}].models.{name} would store a secret \
                                 in a file; model names must not look like credential keys",
                                path.display()
                            )));
                        }
                        let fields = entry.as_table().ok_or_else(|| {
                            UsageError::new(format!(
                                "{}: [providers.{id}.models.{name}] must be a table, \
                                 found {}",
                                path.display(),
                                entry.type_str()
                            ))
                        })?;
                        let mut model = ModelFile::default();
                        for (field, value) in fields {
                            let reader = Reader {
                                path,
                                layer,
                                section: &format!("providers.{id}.models.{name}"),
                                key: field,
                                value,
                            };
                            match field.as_str() {
                                "context_window" => {
                                    model.context_window = Some(reader.integer()?);
                                }
                                "max_output_tokens" => {
                                    model.max_output_tokens = Some(reader.integer()?);
                                }
                                "reasoning" => model.reasoning = Some(reader.string_array()?),
                                "input" => model.input = Some(reader.string_array()?),
                                "output" => model.output = Some(reader.string_array()?),
                                _ => config.warnings.push(format!(
                                    "{}: unknown key [providers.{id}.models.{name}].\
                                     {field}; ignored",
                                    path.display()
                                )),
                            }
                        }
                        provider.models.insert(name.clone(), model);
                    }
                }
                "cache" => {
                    let table = value.as_table().ok_or_else(|| {
                        UsageError::new(format!(
                            "{}: [providers.{id}].cache must be a table, found {}",
                            path.display(),
                            value.type_str()
                        ))
                    })?;
                    let mut cache = CacheFile::default();
                    for (key, value) in table {
                        let reader = Reader {
                            path,
                            layer,
                            section: &format!("providers.{id}.cache"),
                            key,
                            value,
                        };
                        match key.as_str() {
                            "retention" => cache.retention = Some(reader.string()?),
                            "hints" => cache.hints = Some(reader.string_array()?),
                            _ => config.warnings.push(format!(
                                "{}: unknown key [providers.{id}.cache].{key}; ignored",
                                path.display()
                            )),
                        }
                    }
                    provider.cache = Some(cache);
                }
                _ => config.warnings.push(format!(
                    "{}: unknown key [providers.{id}].{key}; ignored",
                    path.display()
                )),
            }
        }
        config.providers.insert(
            id.to_owned(),
            Sourced {
                value: provider,
                layer,
            },
        );
    }
    Ok(())
}

fn valid_provider_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

/// Alias names become command arguments; they must be single words without
/// separators that would confuse composite ids.
fn validate_alias_name(path: &Path, layer: Layer, name: &str) -> Result<(), UsageError> {
    let valid = !name.trim().is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        && name
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric());
    if valid {
        Ok(())
    } else {
        Err(UsageError::new(format!(
            "{}: alias '{name}' in the {} config must be a single word of letters, digits, \
             '-' or '_' starting with a letter or digit",
            path.display(),
            layer.name()
        )))
    }
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

    /// The sanctioned literal credential. Empty values are refused; the text
    /// is wrapped so no diagnostic path can echo it.
    fn secret(&self) -> Result<Sourced<FileSecret>, UsageError> {
        let sourced = self.string()?;
        Ok(Sourced {
            value: FileSecret(sourced.value),
            layer: sourced.layer,
        })
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
                self.key,
            )));
        }
        Ok(Sourced {
            value,
            layer: self.layer,
        })
    }

    /// Like [`Reader::integer`] but accepts `0` (used where zero disables a
    /// bound).
    fn non_negative_integer(&self) -> Result<Sourced<i64>, UsageError> {
        let value = self
            .value
            .as_integer()
            .ok_or_else(|| self.wrong_type("a non-negative integer"))?;
        if value < 0 {
            return Err(UsageError::new(format!(
                "{}: [{}].{} must be a non-negative integer, found {value}",
                self.path.display(),
                self.section,
                self.key,
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

    /// A non-empty array of non-empty strings (used by
    /// `[providers.<id>.cache].hints`).
    fn string_array(&self) -> Result<Sourced<Vec<String>>, UsageError> {
        let items = self
            .value
            .as_array()
            .ok_or_else(|| self.wrong_type("an array of strings"))?;
        let mut values = Vec::new();
        for item in items {
            let value = item.as_str().ok_or_else(|| {
                UsageError::new(format!(
                    "{}: [{}].{} must contain strings, found {}",
                    self.path.display(),
                    self.section,
                    self.key,
                    item.type_str()
                ))
            })?;
            if value.trim().is_empty() {
                return Err(UsageError::new(format!(
                    "{}: [{}].{} must not contain empty entries",
                    self.path.display(),
                    self.section,
                    self.key
                )));
            }
            values.push(value.to_owned());
        }
        if values.is_empty() {
            return Err(UsageError::new(format!(
                "{}: [{}].{} must not be empty; omit the key instead",
                self.path.display(),
                self.section,
                self.key
            )));
        }
        Ok(Sourced {
            value: values,
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
