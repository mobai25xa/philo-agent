//! Configuration resolution: flag > env > profile default. Secrets stay in
//! the process environment; the CLI passes the variable name to the SDK and
//! never reads or prints the value.

use std::path::PathBuf;

use philo_agent_runtime::ReasoningEffort;
use philo_model::ModelProtocol;

/// Environment variable holding the API key; resolved by the SDK credential
/// contract at call time.
pub const API_KEY_ENV: &str = "PHILO_API_KEY";

/// A user-facing configuration error: printed to stderr, exit code 2.
#[derive(Debug, PartialEq, Eq)]
pub struct UsageError(pub String);

impl UsageError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Model deployment configuration (composition-root owned).
#[derive(Debug)]
pub struct Deployment {
    pub provider: String,
    pub protocol: ModelProtocol,
    pub model: String,
    pub endpoint: String,
}

/// Resolves the deployment from flags and environment.
pub fn resolve_deployment(model_flag: Option<&str>) -> Result<Deployment, UsageError> {
    let model = match model_flag {
        Some(model) => model.to_owned(),
        None => std::env::var("PHILO_MODEL")
            .map_err(|_| UsageError::new("no model configured: pass --model or set PHILO_MODEL"))?,
    };
    let endpoint = std::env::var("PHILO_ENDPOINT").map_err(|_| {
        UsageError::new("no endpoint configured: set PHILO_ENDPOINT to the full request URL")
    })?;
    let protocol = match std::env::var("PHILO_PROTOCOL") {
        Err(_) => ModelProtocol::OpenAiChatCompatible,
        Ok(value) => parse_protocol(&value)?,
    };
    let provider = std::env::var("PHILO_PROVIDER").unwrap_or_else(|_| "philo-cli".to_owned());
    Ok(Deployment {
        provider,
        protocol,
        model,
        endpoint,
    })
}

pub fn parse_protocol(value: &str) -> Result<ModelProtocol, UsageError> {
    match value {
        "anthropic-messages" => Ok(ModelProtocol::AnthropicMessages),
        "openai-chat" => Ok(ModelProtocol::OpenAiChat),
        "openai-chat-compatible" => Ok(ModelProtocol::OpenAiChatCompatible),
        "openai-chat-reasoning-content" => Ok(ModelProtocol::OpenAiChatReasoningContent),
        "openai-responses" => Ok(ModelProtocol::OpenAiResponses),
        other => Err(UsageError::new(format!(
            "unknown PHILO_PROTOCOL '{other}': expected anthropic-messages | openai-chat | \
             openai-chat-compatible | openai-chat-reasoning-content | openai-responses"
        ))),
    }
}

pub fn parse_reasoning_effort(value: &str) -> Result<ReasoningEffort, UsageError> {
    match value {
        "minimal" => Ok(ReasoningEffort::Minimal),
        "low" => Ok(ReasoningEffort::Low),
        "medium" => Ok(ReasoningEffort::Medium),
        "high" => Ok(ReasoningEffort::High),
        "very-high" => Ok(ReasoningEffort::VeryHigh),
        "maximum" => Ok(ReasoningEffort::Maximum),
        other => Err(UsageError::new(format!(
            "invalid --reasoning-effort '{other}': expected minimal | low | medium | high | \
             very-high | maximum"
        ))),
    }
}

/// Resolves the session data directory: flag > PHILO_DATA_DIR > ~/.philo/sessions.
pub fn resolve_data_dir(flag: Option<PathBuf>) -> Result<PathBuf, UsageError> {
    if let Some(dir) = flag {
        return Ok(dir);
    }
    if let Ok(dir) = std::env::var("PHILO_DATA_DIR") {
        return Ok(PathBuf::from(dir));
    }
    dirs::home_dir()
        .map(|home| home.join(".philo").join("sessions"))
        .ok_or_else(|| {
            UsageError::new(
                "cannot determine the home directory: pass --data-dir or set PHILO_DATA_DIR",
            )
        })
}

/// Generates a filesystem-encoding-friendly fresh session id.
pub fn generate_session_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    format!("sess-{millis:x}-{}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_effort_parses_all_six_levels_and_rejects_garbage() {
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
    }

    #[test]
    fn protocol_parses_the_live_smoke_vocabulary() {
        assert_eq!(
            parse_protocol("openai-chat-reasoning-content").unwrap(),
            ModelProtocol::OpenAiChatReasoningContent
        );
        assert!(parse_protocol("grpc").is_err());
    }

    #[test]
    fn generated_session_ids_are_filesystem_friendly() {
        let id = generate_session_id();
        assert!(id.starts_with("sess-"));
        assert!(
            id.bytes()
                .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-'))
        );
    }
}
