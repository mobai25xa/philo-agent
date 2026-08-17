use std::error::Error;
use std::fmt;
use std::sync::Arc;

use philo::api::extension as ext;
use philo::api::extension::ProtocolAdapter;
use philo::api::stable as sdk;
use philo_agent_runtime::ReasoningEffort;
use url::Url;

use crate::headers::{ModelRequestHeaders, default_provider_headers};
use crate::replay::ModelReplayStore;

use crate::adapter::PhiloModelAdapter;

/// Conversation continuation policy for one model deployment.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ModelContinuationPolicy {
    /// Keep provider requests stateless and reconstruct the full history from
    /// the private local replay sidecar.
    #[default]
    StatelessLocalReplay,
    /// Prefer a target-bound `previous_response_id` chain, with one local
    /// stateless fallback when the provider reports that the chain vanished.
    PreferPreviousResponseIdWithLocalFallback,
}

/// Built-in protocol selection for the standard assembly.
///
/// Provider and protocol choice is runtime configuration expressed through
/// the SDK `CallTarget`. Chat / Responses provider differences live on
/// [`ModelCompat`] (and an optional Chat [`ChatReasoningFormat`]), not on
/// extra protocol variants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelProtocol {
    /// OpenAI Chat Completions (`openai-chat/v2`).
    OpenAiChat,
    /// OpenAI Responses (`openai-responses/v2`).
    OpenAiResponses,
}

impl ModelProtocol {
    /// Returns the public configuration name for this protocol.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiChat => "openai-chat",
            Self::OpenAiResponses => "openai-responses",
        }
    }

    /// Returns whether the assembled protocol × compat × Chat reasoning
    /// format accepts the requested reasoning-effort level.
    ///
    /// Chat supports all six levels when the effective format is
    /// `EffortOnly` or `EffortAndContent`. Responses supports all six
    /// levels only with official compat. An omitted Chat format follows
    /// the SDK preset (`EffortAndContent` for both `default()` and
    /// `compatible()`).
    #[must_use]
    pub const fn supports_reasoning_effort(
        self,
        compat: ModelCompat,
        chat_reasoning_format: Option<ChatReasoningFormat>,
        effort: ReasoningEffort,
    ) -> bool {
        let _ = effort;
        match self {
            Self::OpenAiChat => matches!(
                effective_chat_reasoning_format(chat_reasoning_format),
                ChatReasoningFormat::EffortOnly | ChatReasoningFormat::EffortAndContent
            ),
            Self::OpenAiResponses => matches!(compat, ModelCompat::Official),
        }
    }

    fn protocol_id(self) -> sdk::ProtocolId {
        let id = match self {
            Self::OpenAiChat => sdk::ProtocolId::OPENAI_CHAT_V2,
            Self::OpenAiResponses => sdk::ProtocolId::OPENAI_RESPONSES_V2,
        };
        sdk::ProtocolId::new(id).expect("built-in protocol id is valid")
    }
}

/// Explicit Chat / Responses compatibility preset. Unknown third-party
/// deployments default to [`Compatible`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ModelCompat {
    Official,
    #[default]
    Compatible,
}

impl ModelCompat {
    /// Returns the public configuration name for this compat preset.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Official => "official",
            Self::Compatible => "compatible",
        }
    }
}

/// Optional Chat-only override of `OpenAiChatReasoningFormat`.
///
/// Omitted at assembly time, the value follows the selected compat preset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatReasoningFormat {
    None,
    EffortOnly,
    ContentOnly,
    EffortAndContent,
}

impl ChatReasoningFormat {
    /// Returns the public configuration name for this Chat reasoning format.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::EffortOnly => "effort-only",
            Self::ContentOnly => "content-only",
            Self::EffortAndContent => "effort-and-content",
        }
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
    request_headers: ModelRequestHeaders,
    replay_store: Option<Arc<dyn ModelReplayStore>>,
    compat: ModelCompat,
    chat_reasoning_format: Option<ChatReasoningFormat>,
    continuation_policy: ModelContinuationPolicy,
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
            request_headers: ModelRequestHeaders::new(),
            replay_store: None,
            compat: ModelCompat::default(),
            chat_reasoning_format: None,
            continuation_policy: ModelContinuationPolicy::default(),
            retry: None,
            timeouts: None,
        }
    }

    /// Names the environment variable holding the API key.
    pub fn api_key_env(mut self, variable: impl Into<String>) -> Self {
        self.api_key_env = Some(variable.into());
        self
    }

    /// Configures validated, non-credential headers for this endpoint binding.
    pub fn request_headers(mut self, headers: ModelRequestHeaders) -> Self {
        self.request_headers = headers;
        self
    }

    /// Injects the provider replay sidecar used for restart-safe history.
    pub fn replay_store(mut self, store: Arc<dyn ModelReplayStore>) -> Self {
        self.replay_store = Some(store);
        self
    }

    /// Selects the Chat / Responses compat preset. The default is compatible.
    pub fn compat(mut self, compat: ModelCompat) -> Self {
        self.compat = compat;
        self
    }

    /// Overrides Chat `reasoning_format` on top of the selected compat preset.
    ///
    /// Omit this setter to keep the preset default. Setting it on Responses
    /// fails assembly.
    pub fn chat_reasoning_format(mut self, format: ChatReasoningFormat) -> Self {
        self.chat_reasoning_format = Some(format);
        self
    }

    /// Selects whether the deployment may use provider-stored response
    /// continuation. The default is fully stateless local replay.
    pub fn continuation_policy(mut self, policy: ModelContinuationPolicy) -> Self {
        self.continuation_policy = policy;
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
        let transport = ext::ReqwestTransport::builder()
            .proxy_policy(ext::ProxyPolicy::System)
            .build()
            .map_err(|error| {
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
        let prefers_continuation = self.continuation_policy
            == ModelContinuationPolicy::PreferPreviousResponseIdWithLocalFallback;

        if prefers_continuation && self.protocol != ModelProtocol::OpenAiResponses {
            return Err(AdapterBuildError::new(
                "previous_response_id continuation requires the OpenAI Responses protocol",
            ));
        }
        if self.chat_reasoning_format.is_some() && self.protocol != ModelProtocol::OpenAiChat {
            return Err(AdapterBuildError::new(
                "reasoning_format is only valid for the OpenAI Chat protocol",
            ));
        }
        if prefers_continuation
            && self.compat == ModelCompat::Official
            && endpoint.host_str() != Some("api.openai.com")
        {
            return Err(AdapterBuildError::new(
                "official OpenAI continuation support requires the api.openai.com endpoint host",
            ));
        }

        let client_builder = sdk::PhiloClient::builder().transport(transport);
        let (client_builder, envelope, model_compat) = match self.protocol {
            ModelProtocol::OpenAiChat => {
                let compat = chat_compat(self.compat, self.chat_reasoning_format);
                let adapter = ext::OpenAiChatAdapter::with_compat(compat.clone());
                let envelope = adapter.capabilities().envelope().clone();
                (
                    client_builder.register_protocol(adapter),
                    envelope,
                    ext::ProtocolCompat::from(compat),
                )
            }
            ModelProtocol::OpenAiResponses => {
                let compat = responses_compat(self.compat, prefers_continuation);
                let adapter = ext::OpenAiResponsesAdapter::with_compat(compat.clone());
                let envelope = adapter.capabilities().envelope().clone();
                (
                    client_builder.register_protocol(adapter),
                    envelope,
                    ext::ProtocolCompat::from(compat),
                )
            }
        };

        let mut constraints = ext::CapabilityConstraints::default();
        if !prefers_continuation {
            constraints
                .disabled
                .insert(sdk::Capability::ResponseContinuation);
        }
        let binding = ext::ProviderBinding::new(protocol_id.clone(), endpoint, constraints)
            .map_err(|error| AdapterBuildError::new(format!("provider binding invalid: {error}")))?
            .with_headers(self.request_headers.provider_headers());
        let model_profile = ext::ModelProfile::new(
            protocol_id.clone(),
            model_name.clone(),
            ext::ModelCapabilities::new(envelope),
            ext::CapabilityConstraints::default(),
            sdk::CapabilityKnowledge::Complete,
            sdk::CapabilitySource::new("philo-model assembly").expect("static source is valid"),
            ext::ModelMetadata::default(),
        )
        .with_compat(model_compat);
        let mut provider = ext::ProviderProfile::new(provider_id.clone())
            .with_default_headers(default_provider_headers())
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
        let target = sdk::CallTarget::new(provider_id, protocol_id, model_name);
        let replay_store = self
            .replay_store
            .unwrap_or_else(|| Arc::new(crate::replay::MemoryModelReplayStore::default()));
        Ok(PhiloModelAdapter::with_configuration(
            client,
            target,
            replay_store,
            self.continuation_policy,
        ))
    }
}

const fn effective_chat_reasoning_format(
    chat_reasoning_format: Option<ChatReasoningFormat>,
) -> ChatReasoningFormat {
    match chat_reasoning_format {
        Some(format) => format,
        // Both `OpenAiChatCompat::default()` and `compatible()` use EffortAndContent.
        None => ChatReasoningFormat::EffortAndContent,
    }
}

fn chat_compat(compat: ModelCompat, format: Option<ChatReasoningFormat>) -> ext::OpenAiChatCompat {
    let preset = match compat {
        ModelCompat::Official => ext::OpenAiChatCompat::default(),
        // SDK `compatible()` stays conservative (no cache identity) so unknown
        // gateways do not 400. The agent opts in: newapi-class endpoints need
        // a stable session key plus OpenAI-format affinity headers to pin the
        // prefix to one cache replica. `include_usage` is required to observe
        // `cached_tokens` on OpenAI-shaped streams.
        ModelCompat::Compatible => ext::OpenAiChatCompat::compatible()
            .with_cache_key(ext::OpenAiChatCacheKey::PromptCacheKey)
            .with_session_affinity(ext::OpenAiChatSessionAffinity::OpenAi)
            .with_sends_stream_usage(true),
    };
    match format {
        None => preset,
        Some(ChatReasoningFormat::None) => {
            preset.with_reasoning_format(ext::OpenAiChatReasoningFormat::None)
        }
        Some(ChatReasoningFormat::EffortOnly) => {
            preset.with_reasoning_format(ext::OpenAiChatReasoningFormat::EffortOnly)
        }
        Some(ChatReasoningFormat::ContentOnly) => preset
            .with_reasoning_format(ext::OpenAiChatReasoningFormat::ContentOnly)
            .with_unknown_fields(ext::OpenAiChatUnknownFieldPolicy::Reject),
        Some(ChatReasoningFormat::EffortAndContent) => {
            preset.with_reasoning_format(ext::OpenAiChatReasoningFormat::EffortAndContent)
        }
    }
}

fn responses_compat(compat: ModelCompat, prefers_continuation: bool) -> ext::OpenAiResponsesCompat {
    match (compat, prefers_continuation) {
        (ModelCompat::Official, _) => ext::OpenAiResponsesCompat::default(),
        // SDK `compatible()` stays conservative (no cache identity) so unknown
        // gateways do not 400. The agent opts in: newapi-class endpoints need
        // a stable session key plus OpenAI-format affinity headers to pin the
        // prefix to one cache replica.
        (ModelCompat::Compatible, false) => ext::OpenAiResponsesCompat::compatible()
            .with_cache_key(ext::OpenAiResponsesCacheKey::PromptCacheKey)
            .with_session_affinity(ext::OpenAiResponsesSessionAffinity::OpenAi),
        (ModelCompat::Compatible, true) => ext::OpenAiResponsesCompat::compatible()
            .with_continuation(true)
            .with_cache_key(ext::OpenAiResponsesCacheKey::PromptCacheKey)
            .with_session_affinity(ext::OpenAiResponsesSessionAffinity::OpenAi),
    }
}
