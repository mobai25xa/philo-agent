use std::error::Error;
use std::fmt;

use philo::api::extension as ext;
use philo::api::extension::ProtocolAdapter;
use philo::api::stable as sdk;
use url::Url;

use crate::adapter::PhiloModelAdapter;

/// Built-in protocol selection for the standard assembly.
///
/// Provider and protocol choice is runtime configuration expressed through
/// the SDK `CallTarget`; no per-vendor crate exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelProtocol {
    /// Anthropic Messages (`anthropic-messages/2023-06-01`).
    AnthropicMessages,
    /// OpenAI Chat Completions, official OpenAI shape (`openai-chat/openai/v1`).
    OpenAiChat,
    /// OpenAI Chat Completions, conservative compatible shape
    /// (`openai-chat/compatible/v2`) for OpenAI-compatible providers.
    OpenAiChatCompatible,
    /// OpenAI Chat Completions, `reasoning_content` dialect
    /// (`openai-chat/reasoning-content/v1`) for OpenAI-compatible providers
    /// that stream visible reasoning through `delta.reasoning_content`.
    OpenAiChatReasoningContent,
    /// OpenAI Responses (`openai-responses/v1`).
    OpenAiResponses,
}

impl ModelProtocol {
    fn protocol_id(self) -> sdk::ProtocolId {
        let id = match self {
            Self::AnthropicMessages => sdk::ProtocolId::ANTHROPIC_MESSAGES_2023_06_01,
            Self::OpenAiChat => sdk::ProtocolId::OPENAI_CHAT_OPENAI_V1,
            Self::OpenAiChatCompatible => sdk::ProtocolId::OPENAI_CHAT_COMPATIBLE_V2,
            Self::OpenAiChatReasoningContent => sdk::ProtocolId::OPENAI_CHAT_REASONING_CONTENT_V1,
            Self::OpenAiResponses => sdk::ProtocolId::OPENAI_RESPONSES_OPENAI_V1,
        };
        sdk::ProtocolId::new(id).expect("built-in protocol id is valid")
    }
}

/// Assembly failure with a redacted, human-readable reason.
#[derive(Clone, Debug)]
pub struct AdapterBuildError {
    message: String,
}

impl AdapterBuildError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Returns the failure description.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for AdapterBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for AdapterBuildError {}

/// Assembles a `PhiloClient` and `CallTarget` for one provider deployment.
///
/// The API key is referenced by environment-variable name and resolved by the
/// SDK credential contract at call time; the secret value is never read,
/// stored, or logged by this crate.
#[derive(Clone, Debug)]
pub struct PhiloModelBuilder {
    provider: String,
    protocol: ModelProtocol,
    model: String,
    endpoint: String,
    api_key_env: Option<String>,
    retry: Option<sdk::RetryPolicy>,
    timeouts: Option<sdk::TimeoutPolicy>,
}

impl PhiloModelBuilder {
    /// Creates a builder for one provider/protocol/model deployment with the
    /// full request endpoint URL.
    pub fn new(
        provider: impl Into<String>,
        protocol: ModelProtocol,
        model: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            protocol,
            model: model.into(),
            endpoint: endpoint.into(),
            api_key_env: None,
            retry: None,
            timeouts: None,
        }
    }

    /// Names the environment variable holding the API key.
    pub fn api_key_env(mut self, variable: impl Into<String>) -> Self {
        self.api_key_env = Some(variable.into());
        self
    }

    /// Configures the SDK attempt-level bounded retry policy.
    pub fn retry_policy(mut self, policy: sdk::RetryPolicy) -> Self {
        self.retry = Some(policy);
        self
    }

    /// Configures the SDK timeout policy.
    pub fn timeout_policy(mut self, policy: sdk::TimeoutPolicy) -> Self {
        self.timeouts = Some(policy);
        self
    }

    /// Builds the adapter over the default reqwest transport.
    pub fn build(self) -> Result<PhiloModelAdapter, AdapterBuildError> {
        let transport = ext::ReqwestTransport::builder().build().map_err(|error| {
            AdapterBuildError::new(format!("transport assembly failed: {error}"))
        })?;
        self.build_with_transport(transport)
    }

    /// Builds the adapter over an explicit SDK transport. This is also the
    /// injection point tests use for scripted transports; stub transports
    /// themselves live only in test support code.
    pub fn build_with_transport<T: ext::Transport + 'static>(
        self,
        transport: T,
    ) -> Result<PhiloModelAdapter, AdapterBuildError> {
        let provider_id = sdk::ProviderId::new(&self.provider)
            .map_err(|error| AdapterBuildError::new(format!("invalid provider id: {error}")))?;
        let model_name = sdk::ModelName::new(&self.model)
            .map_err(|error| AdapterBuildError::new(format!("invalid model name: {error}")))?;
        let endpoint = Url::parse(&self.endpoint)
            .map_err(|error| AdapterBuildError::new(format!("invalid endpoint url: {error}")))?;
        let protocol_id = self.protocol.protocol_id();

        let client_builder = sdk::PhiloClient::builder().transport(transport);
        let (client_builder, envelope) = match self.protocol {
            ModelProtocol::AnthropicMessages => {
                let adapter = ext::AnthropicMessagesAdapter::default();
                let envelope = adapter.capabilities().envelope().clone();
                (client_builder.register_protocol(adapter), envelope)
            }
            ModelProtocol::OpenAiChat => {
                let adapter = ext::OpenAiChatAdapter::openai_v1();
                let envelope = adapter.capabilities().envelope().clone();
                (client_builder.register_protocol(adapter), envelope)
            }
            ModelProtocol::OpenAiChatCompatible => {
                let adapter = ext::OpenAiChatAdapter::compatible_v2();
                let envelope = adapter.capabilities().envelope().clone();
                (client_builder.register_protocol(adapter), envelope)
            }
            ModelProtocol::OpenAiChatReasoningContent => {
                let adapter = ext::OpenAiChatAdapter::reasoning_content_v1();
                let envelope = adapter.capabilities().envelope().clone();
                (client_builder.register_protocol(adapter), envelope)
            }
            ModelProtocol::OpenAiResponses => {
                let adapter = ext::OpenAiResponsesAdapter::default();
                let envelope = adapter.capabilities().envelope().clone();
                (client_builder.register_protocol(adapter), envelope)
            }
        };

        let binding = ext::ProviderBinding::new(
            protocol_id.clone(),
            endpoint,
            ext::CapabilityConstraints::default(),
        )
        .map_err(|error| AdapterBuildError::new(format!("provider binding invalid: {error}")))?;
        let model_profile = ext::ModelProfile::new(
            protocol_id.clone(),
            model_name.clone(),
            ext::ModelCapabilities::new(envelope),
            sdk::CapabilityKnowledge::Complete,
            sdk::CapabilitySource::new("philo-model assembly").expect("static source is valid"),
            ext::ModelMetadata::default(),
        );
        let mut provider = ext::ProviderProfile::new(provider_id.clone())
            .add_binding(binding)
            .map_err(|error| AdapterBuildError::new(format!("provider binding rejected: {error}")))?
            .add_model(model_profile)
            .map_err(|error| AdapterBuildError::new(format!("model profile rejected: {error}")))?;
        if let Some(variable) = &self.api_key_env {
            let name = ext::EnvVarName::new(variable.as_str()).map_err(|error| {
                AdapterBuildError::new(format!("invalid credential variable name: {error}"))
            })?;
            provider = provider.with_credentials(ext::CredentialSpec::Environment(name));
        }

        let mut client_builder = client_builder.register_provider(provider);
        if let Some(retry) = self.retry {
            client_builder = client_builder.retry_policy(retry);
        }
        if let Some(timeouts) = self.timeouts {
            client_builder = client_builder.timeout_policy(timeouts);
        }
        let client = client_builder
            .build()
            .map_err(|error| AdapterBuildError::new(format!("client assembly failed: {error}")))?;
        Ok(PhiloModelAdapter::new(
            client,
            sdk::CallTarget::new(provider_id, protocol_id, model_name),
        ))
    }
}
