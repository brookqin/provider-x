mod custom;
mod deepseek;

use std::collections::BTreeMap;

use bytes::Bytes;
use hyper::{HeaderMap, header, header::HeaderValue};
use protocol_openai_chat_completions::ToolIdentity;
use provider_x_core::{
    AnthropicThinkingMode, AuthConfig, DiscoveredModel, ModelCacheDocument, ProtocolId,
    ProviderConfig, ProviderKind, ProviderModelSource, ProvidersDocument, RuntimeSnapshot,
    TransportConfig,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub use deepseek::{DEEPSEEK_HTTP_ENDPOINT, DEEPSEEK_MODELS_DEV_ID, DEEPSEEK_MODELS_ENDPOINT};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderDefinition {
    pub kind: ProviderKind,
    pub display_name: &'static str,
    pub default_namespace: &'static str,
    pub models_dev_id: Option<&'static str>,
    pub configurable: bool,
    pub protocol: ProtocolId,
    pub http_endpoint: &'static str,
    pub models_endpoint: Option<&'static str>,
    pub http_sse: bool,
    pub websocket: bool,
}

pub const PROVIDER_DEFINITIONS: &[ProviderDefinition] = &[deepseek::DEFINITION, custom::DEFINITION];

#[must_use]
pub fn provider_definition(kind: ProviderKind) -> &'static ProviderDefinition {
    match kind {
        ProviderKind::DeepSeek => &deepseek::DEFINITION,
        ProviderKind::Custom => &custom::DEFINITION,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AuthStyle {
    Bearer,
    Anthropic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ProtocolAdapter {
    OpenaiResponses,
    OpenaiChatCompletions,
    AnthropicMessages,
}

impl ProtocolAdapter {
    const fn protocol(self) -> ProtocolId {
        match self {
            Self::OpenaiResponses => ProtocolId::OpenaiResponses,
            Self::OpenaiChatCompletions => ProtocolId::OpenaiChatCompletions,
            Self::AnthropicMessages => ProtocolId::AnthropicMessages,
        }
    }
}

impl From<ProtocolId> for ProtocolAdapter {
    fn from(protocol: ProtocolId) -> Self {
        match protocol {
            ProtocolId::OpenaiResponses => Self::OpenaiResponses,
            ProtocolId::OpenaiChatCompletions => Self::OpenaiChatCompletions,
            ProtocolId::AnthropicMessages => Self::AnthropicMessages,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WsHttpAdapterKind {
    OpenaiResponses,
    OpenaiChatCompletions,
    AnthropicMessages,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebSocketPlan {
    Direct,
    HttpBridge(WsHttpAdapterKind),
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpTarget {
    PreserveIngressPath(String),
    Exact(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpResponseAdapter {
    Passthrough,
    OpenaiChatCompletions(BTreeMap<String, ToolIdentity>),
    AnthropicMessages(BTreeMap<String, ToolIdentity>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedHttpRequest {
    pub target: HttpTarget,
    pub body: Bytes,
    pub response_adapter: HttpResponseAdapter,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderProfile {
    kind: ProviderKind,
    implementation_id: &'static str,
    implementation_revision: u32,
    adapter: ProtocolAdapter,
    auth_style: AuthStyle,
    http_endpoint: String,
    websocket_endpoint: Option<String>,
    models_endpoint: Option<String>,
    transports: TransportConfig,
    models_dev_id: Option<&'static str>,
    anthropic_thinking: AnthropicThinkingMode,
    accepts_legacy_fingerprint: bool,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error(transparent)]
    Core(#[from] provider_x_core::CoreError),

    #[error("provider request conversion failed: {0}")]
    RequestConversion(String),

    #[error("provider model discovery response is invalid: {0}")]
    ModelDiscovery(String),

    #[error("provider authentication header is invalid")]
    InvalidAuthenticationHeader,

    #[error("failed to serialize provider routing semantics: {0}")]
    FingerprintSerialization(String),

    #[error("provider WebSocket request conversion is unavailable")]
    WebSocketConversionUnavailable,

    #[error("provider {provider_id} does not match the fixed {implementation} configuration")]
    DedicatedConfigurationMismatch {
        provider_id: String,
        implementation: &'static str,
    },
}

#[must_use]
pub fn resolve_provider(provider: &ProviderConfig) -> ProviderProfile {
    match provider.kind {
        ProviderKind::DeepSeek => deepseek::profile(),
        ProviderKind::Custom => custom::profile(provider),
    }
}

/// Validates common configuration and the fixed fields of dedicated implementations.
///
/// # Errors
///
/// Returns an error when common configuration is invalid or a dedicated provider contains
/// protocol/endpoint fields that disagree with its registry definition.
pub fn validate_provider(provider: &ProviderConfig) -> Result<(), ProviderError> {
    provider.validate()?;
    let definition = provider_definition(provider.kind);
    if !definition.configurable
        && (provider.protocol != definition.protocol
            || provider.anthropic_thinking.is_some()
            || provider.endpoints.http != definition.http_endpoint
            || provider.endpoints.websocket.is_some()
            || provider.endpoints.models.as_deref() != definition.models_endpoint
            || provider.transports.http_sse != definition.http_sse
            || provider.transports.websocket != definition.websocket)
    {
        return Err(ProviderError::DedicatedConfigurationMismatch {
            provider_id: provider.id.to_string(),
            implementation: definition.display_name,
        });
    }
    Ok(())
}

/// Validates the document plus every dedicated provider implementation.
///
/// # Errors
///
/// Returns the first document or provider implementation error.
pub fn validate_document(providers: &ProvidersDocument) -> Result<(), ProviderError> {
    providers.validate()?;
    for provider in &providers.providers {
        validate_provider(provider)?;
    }
    Ok(())
}

/// Tests a cache fingerprint against the provider's effective implementation semantics.
///
/// # Errors
///
/// Returns an error if fingerprint serialization fails.
pub fn cache_fingerprint_matches(
    provider: &ProviderConfig,
    candidate: &str,
) -> Result<bool, provider_x_core::CoreError> {
    resolve_provider(provider)
        .matches_cache_fingerprint(provider, candidate)
        .map_err(|error| provider_x_core::CoreError::FingerprintSerialization(error.to_string()))
}

/// Builds routing state using effective provider implementation fingerprints.
///
/// # Errors
///
/// Returns an error for invalid configuration, cache, or provider semantics.
pub fn build_runtime_snapshot(
    providers: &ProvidersDocument,
    cache: &ModelCacheDocument,
) -> Result<RuntimeSnapshot, ProviderError> {
    validate_document(providers)?;
    RuntimeSnapshot::build_with_fingerprint_matcher(providers, cache, cache_fingerprint_matches)
        .map_err(Into::into)
}

impl ProviderProfile {
    #[must_use]
    pub const fn kind(&self) -> ProviderKind {
        self.kind
    }

    #[must_use]
    pub const fn protocol(&self) -> ProtocolId {
        self.adapter.protocol()
    }

    #[must_use]
    pub fn http_endpoint(&self) -> &str {
        &self.http_endpoint
    }

    #[must_use]
    pub fn websocket_endpoint(&self) -> Option<&str> {
        self.websocket_endpoint.as_deref()
    }

    #[must_use]
    pub const fn transports(&self) -> &TransportConfig {
        &self.transports
    }

    #[must_use]
    pub const fn models_dev_id(&self) -> Option<&'static str> {
        self.models_dev_id
    }

    #[must_use]
    pub const fn anthropic_thinking_mode(&self) -> AnthropicThinkingMode {
        self.anthropic_thinking
    }

    #[must_use]
    pub fn model_list_url(&self) -> String {
        self.models_endpoint
            .clone()
            .unwrap_or_else(|| match self.adapter {
                ProtocolAdapter::OpenaiResponses => {
                    protocol_openai_responses::model_list_url(&self.http_endpoint)
                }
                ProtocolAdapter::OpenaiChatCompletions => {
                    protocol_openai_chat_completions::model_list_url(&self.http_endpoint)
                }
                ProtocolAdapter::AnthropicMessages => {
                    protocol_anthropic_messages::model_list_url(&self.http_endpoint)
                }
            })
    }

    #[must_use]
    pub fn model_source(&self) -> ProviderModelSource {
        ProviderModelSource {
            protocol: self.protocol(),
            endpoint: self.model_list_url(),
        }
    }

    /// Parses the model-list payload using the implementation-selected discovery adapter.
    ///
    /// # Errors
    ///
    /// Returns an error when the upstream payload is not a supported model list.
    pub fn parse_model_list(&self, bytes: &[u8]) -> Result<Vec<DiscoveredModel>, ProviderError> {
        match self.kind {
            ProviderKind::DeepSeek => deepseek::parse_model_list(bytes),
            ProviderKind::Custom => custom::parse_model_list(self, bytes),
        }
    }

    fn parse_model_list_by_adapter(
        &self,
        bytes: &[u8],
    ) -> Result<Vec<DiscoveredModel>, ProviderError> {
        match self.adapter {
            ProtocolAdapter::OpenaiResponses => protocol_openai_responses::parse_model_list(bytes)
                .map_err(|error| ProviderError::ModelDiscovery(error.to_string())),
            ProtocolAdapter::OpenaiChatCompletions => {
                protocol_openai_chat_completions::parse_model_list(bytes)
                    .map_err(|error| ProviderError::ModelDiscovery(error.to_string()))
            }
            ProtocolAdapter::AnthropicMessages => {
                protocol_anthropic_messages::parse_model_list(bytes)
                    .map_err(|error| ProviderError::ModelDiscovery(error.to_string()))
            }
        }
    }

    /// Applies the selected implementation's upstream authentication headers.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured key cannot be represented as an HTTP header.
    pub fn apply_authentication(
        &self,
        auth: &AuthConfig,
        headers: &mut HeaderMap,
    ) -> Result<(), ProviderError> {
        match self.kind {
            ProviderKind::DeepSeek => deepseek::apply_authentication(auth, headers),
            ProviderKind::Custom => custom::apply_authentication(self, auth, headers),
        }
    }

    fn apply_authentication_by_style(
        &self,
        auth: &AuthConfig,
        headers: &mut HeaderMap,
    ) -> Result<(), ProviderError> {
        match (self.auth_style, auth) {
            (AuthStyle::Bearer, AuthConfig::Bearer { api_key }) => {
                let value = HeaderValue::from_str(&format!("Bearer {api_key}"))
                    .map_err(|_| ProviderError::InvalidAuthenticationHeader)?;
                headers.insert(header::AUTHORIZATION, value);
            }
            (AuthStyle::Anthropic, AuthConfig::Bearer { api_key }) => {
                let value = HeaderValue::from_str(api_key)
                    .map_err(|_| ProviderError::InvalidAuthenticationHeader)?;
                headers.insert("x-api-key", value);
                headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
            }
        }
        Ok(())
    }

    /// Converts a Responses ingress request into the selected upstream request.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed or unsupported Responses input.
    pub fn prepare_http_request(
        &self,
        body: &[u8],
        upstream_model: &str,
        max_bytes: usize,
    ) -> Result<PreparedHttpRequest, ProviderError> {
        match self.kind {
            ProviderKind::DeepSeek => {
                deepseek::prepare_http_request(body, upstream_model, max_bytes)
            }
            ProviderKind::Custom => {
                custom::prepare_http_request(self, body, upstream_model, max_bytes)
            }
        }
    }

    fn prepare_http_request_by_adapter(
        &self,
        body: &[u8],
        upstream_model: &str,
        max_bytes: usize,
    ) -> Result<PreparedHttpRequest, ProviderError> {
        match self.adapter {
            ProtocolAdapter::OpenaiResponses => Ok(PreparedHttpRequest {
                target: HttpTarget::PreserveIngressPath(self.http_endpoint.clone()),
                body: protocol_openai_responses::rewrite_http_model(body, upstream_model)
                    .map_err(|error| ProviderError::RequestConversion(error.to_string()))?,
                response_adapter: HttpResponseAdapter::Passthrough,
            }),
            ProtocolAdapter::OpenaiChatCompletions => {
                let request = protocol_openai_chat_completions::prepare_http_request(
                    body,
                    upstream_model,
                    max_bytes,
                )
                .map_err(|error| ProviderError::RequestConversion(error.to_string()))?;
                Ok(PreparedHttpRequest {
                    target: HttpTarget::Exact(
                        protocol_openai_chat_completions::chat_completions_url(&self.http_endpoint),
                    ),
                    body: request.body,
                    response_adapter: HttpResponseAdapter::OpenaiChatCompletions(
                        request.tool_names,
                    ),
                })
            }
            ProtocolAdapter::AnthropicMessages => {
                let request = protocol_anthropic_messages::prepare_http_request_with_thinking_mode(
                    body,
                    upstream_model,
                    max_bytes,
                    self.anthropic_thinking,
                )
                .map_err(|error| ProviderError::RequestConversion(error.to_string()))?;
                Ok(PreparedHttpRequest {
                    target: HttpTarget::Exact(protocol_anthropic_messages::messages_url(
                        &self.http_endpoint,
                    )),
                    body: request.body,
                    response_adapter: HttpResponseAdapter::AnthropicMessages(request.tool_names),
                })
            }
        }
    }

    #[must_use]
    pub fn websocket_plan(&self) -> WebSocketPlan {
        match self.kind {
            ProviderKind::DeepSeek => deepseek::websocket_plan(),
            ProviderKind::Custom => custom::websocket_plan(self),
        }
    }

    #[must_use]
    pub fn websocket_http_url(&self) -> String {
        match self.kind {
            ProviderKind::DeepSeek => deepseek::websocket_http_url(),
            ProviderKind::Custom => custom::websocket_http_url(self),
        }
    }

    const fn websocket_plan_by_adapter(&self) -> WebSocketPlan {
        match self.adapter {
            ProtocolAdapter::OpenaiResponses if self.transports.websocket => WebSocketPlan::Direct,
            ProtocolAdapter::OpenaiResponses if self.transports.http_sse => {
                WebSocketPlan::HttpBridge(WsHttpAdapterKind::OpenaiResponses)
            }
            ProtocolAdapter::OpenaiChatCompletions if self.transports.http_sse => {
                WebSocketPlan::HttpBridge(WsHttpAdapterKind::OpenaiChatCompletions)
            }
            ProtocolAdapter::AnthropicMessages if self.transports.http_sse => {
                WebSocketPlan::HttpBridge(WsHttpAdapterKind::AnthropicMessages)
            }
            ProtocolAdapter::OpenaiResponses
            | ProtocolAdapter::OpenaiChatCompletions
            | ProtocolAdapter::AnthropicMessages => WebSocketPlan::Unsupported,
        }
    }

    /// Rewrites a direct WebSocket request with the selected implementation adapter.
    ///
    /// # Errors
    ///
    /// Returns an error if direct WebSocket is unavailable or the request is invalid.
    pub fn rewrite_websocket_request(
        &self,
        message: &str,
        upstream_model: &str,
    ) -> Result<String, ProviderError> {
        match self.kind {
            ProviderKind::DeepSeek => deepseek::rewrite_websocket_request(message, upstream_model),
            ProviderKind::Custom => {
                custom::rewrite_websocket_request(self, message, upstream_model)
            }
        }
    }

    fn rewrite_websocket_request_by_adapter(
        &self,
        message: &str,
        upstream_model: &str,
    ) -> Result<String, ProviderError> {
        if self.adapter != ProtocolAdapter::OpenaiResponses {
            return Err(ProviderError::WebSocketConversionUnavailable);
        }
        protocol_openai_responses::rewrite_ws_text(message, upstream_model)
            .map_err(|error| ProviderError::RequestConversion(error.to_string()))
    }

    /// Returns a fingerprint of effective behavior, including the implementation revision.
    ///
    /// # Errors
    ///
    /// Returns an error if semantic fingerprint input serialization fails.
    pub fn routing_fingerprint(&self) -> Result<String, ProviderError> {
        #[derive(Serialize)]
        struct FingerprintInput<'a> {
            kind: ProviderKind,
            implementation_id: &'static str,
            implementation_revision: u32,
            adapter: ProtocolAdapter,
            auth_style: AuthStyle,
            http_endpoint: &'a str,
            websocket_endpoint: Option<&'a str>,
            models_endpoint: String,
            transports: &'a TransportConfig,
            models_dev_id: Option<&'static str>,
            anthropic_thinking: AnthropicThinkingMode,
        }

        let bytes = serde_json::to_vec(&FingerprintInput {
            kind: self.kind,
            implementation_id: self.implementation_id,
            implementation_revision: self.implementation_revision,
            adapter: self.adapter,
            auth_style: self.auth_style,
            http_endpoint: &self.http_endpoint,
            websocket_endpoint: self.websocket_endpoint.as_deref(),
            models_endpoint: self.model_list_url(),
            transports: &self.transports,
            models_dev_id: self.models_dev_id,
            anthropic_thinking: self.anthropic_thinking,
        })
        .map_err(|error| ProviderError::FingerprintSerialization(error.to_string()))?;
        Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
    }

    /// Matches current semantics and the revision-one pre-provider-layer cache fingerprint.
    ///
    /// # Errors
    ///
    /// Returns an error if either fingerprint cannot be serialized.
    pub fn matches_cache_fingerprint(
        &self,
        provider: &ProviderConfig,
        candidate: &str,
    ) -> Result<bool, ProviderError> {
        if candidate == self.routing_fingerprint()? {
            return Ok(true);
        }
        if !self.accepts_legacy_fingerprint {
            return Ok(false);
        }
        let legacy = match self.kind {
            ProviderKind::DeepSeek => deepseek::legacy_routing_fingerprint(provider)?,
            ProviderKind::Custom => provider.routing_fingerprint()?,
        };
        Ok(candidate == legacy)
    }
}

#[cfg(test)]
mod tests {
    use provider_x_core::{
        AuthConfig, EndpointConfig, ProtocolId, ProviderConfig, ProviderId, ProviderKind,
        TransportConfig,
    };

    use super::{
        DEEPSEEK_HTTP_ENDPOINT, DEEPSEEK_MODELS_DEV_ID, DEEPSEEK_MODELS_ENDPOINT, HttpTarget,
        PROVIDER_DEFINITIONS, WebSocketPlan, resolve_provider, validate_provider,
    };

    fn provider(kind: ProviderKind) -> ProviderConfig {
        ProviderConfig {
            id: ProviderId::new("example").unwrap(),
            name: "Example".to_owned(),
            description: None,
            enabled: true,
            kind,
            protocol: ProtocolId::AnthropicMessages,
            anthropic_thinking: None,
            endpoints: EndpointConfig {
                http: "https://custom.example/v1".to_owned(),
                websocket: None,
                models: Some("https://custom.example/v1/models".to_owned()),
            },
            auth: AuthConfig::Bearer {
                api_key: "secret".to_owned(),
            },
            transports: TransportConfig {
                http_sse: true,
                websocket: false,
            },
        }
    }

    #[test]
    fn registry_contains_dedicated_and_custom_implementations() {
        assert_eq!(PROVIDER_DEFINITIONS.len(), 2);
        assert_eq!(PROVIDER_DEFINITIONS[0].models_dev_id, Some("deepseek"));
        assert!(PROVIDER_DEFINITIONS[1].configurable);
    }

    #[test]
    fn deepseek_uses_an_executable_responses_strategy() {
        let profile = resolve_provider(&provider(ProviderKind::DeepSeek));

        assert_eq!(profile.protocol(), ProtocolId::OpenaiResponses);
        assert_eq!(profile.http_endpoint(), DEEPSEEK_HTTP_ENDPOINT);
        assert_eq!(profile.model_list_url(), DEEPSEEK_MODELS_ENDPOINT);
        assert_eq!(profile.models_dev_id(), Some(DEEPSEEK_MODELS_DEV_ID));
        assert_eq!(
            profile.websocket_plan(),
            WebSocketPlan::HttpBridge(super::WsHttpAdapterKind::OpenaiResponses)
        );
        assert_eq!(
            profile.websocket_http_url(),
            "https://api.deepseek.com/v1/responses"
        );

        let prepared = profile
            .prepare_http_request(br#"{"model":"example/coder","input":"hi"}"#, "coder", 4096)
            .unwrap();
        assert_eq!(
            prepared.target,
            HttpTarget::PreserveIngressPath(DEEPSEEK_HTTP_ENDPOINT.to_owned())
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&prepared.body).unwrap()["model"],
            "coder"
        );
    }

    #[test]
    fn custom_provider_preserves_user_selected_strategy() {
        let provider = provider(ProviderKind::Custom);
        let profile = resolve_provider(&provider);

        assert_eq!(profile.protocol(), ProtocolId::AnthropicMessages);
        assert_eq!(profile.http_endpoint(), provider.endpoints.http);
        assert_eq!(profile.websocket_endpoint(), None);
        assert_eq!(profile.model_list_url(), "https://custom.example/v1/models");
        assert_eq!(profile.models_dev_id(), None);
    }

    #[test]
    fn implementation_identity_changes_the_semantic_fingerprint() {
        let custom = provider(ProviderKind::Custom);
        let mut dedicated = custom.clone();
        dedicated.kind = ProviderKind::DeepSeek;

        assert_ne!(
            resolve_provider(&custom).routing_fingerprint().unwrap(),
            resolve_provider(&dedicated).routing_fingerprint().unwrap()
        );
    }

    #[test]
    fn same_protocol_can_use_vendor_specific_bridge_routing() {
        let mut custom = provider(ProviderKind::Custom);
        custom.protocol = ProtocolId::OpenaiResponses;
        custom.endpoints.http = DEEPSEEK_HTTP_ENDPOINT.to_owned();
        let mut dedicated = custom.clone();
        dedicated.kind = ProviderKind::DeepSeek;

        assert_eq!(
            resolve_provider(&custom).websocket_http_url(),
            "https://api.deepseek.com/responses"
        );
        assert_eq!(
            resolve_provider(&dedicated).websocket_http_url(),
            "https://api.deepseek.com/v1/responses"
        );
    }

    #[test]
    fn dedicated_provider_rejects_flat_fields_that_disagree_with_its_registry() {
        let configured = provider(ProviderKind::DeepSeek);

        assert!(validate_provider(&configured).is_err());
    }

    #[test]
    fn migrated_v1_deepseek_is_canonical_for_the_dedicated_implementation() {
        let document = provider_x_core::ProvidersDocument::from_yaml(
            r"
schema_version: 1
listener:
  host: 127.0.0.1
  port: 43119
  request_body_limit_bytes: 1024
  max_connections: 4
timeouts:
  request_body_ms: 1
  connect_ms: 1
  response_headers_ms: 1
  stream_idle_ms: 1
  websocket_idle_ms: 1
  shutdown_grace_ms: 1
codex:
  manage_user_config: false
providers:
  - id: deepseek
    name: DeepSeek
    description: null
    enabled: false
    protocol: openai_responses
    endpoints:
      http: https://api.deepseek.com
      websocket: null
    auth:
      mode: bearer
      api_key: secret
    transports:
      http_sse: true
      websocket: false
",
        )
        .unwrap();

        validate_provider(&document.providers[0]).unwrap();
        assert_eq!(
            document.providers[0].endpoints.models.as_deref(),
            Some(DEEPSEEK_MODELS_ENDPOINT)
        );
        let mut legacy = document.providers[0].clone();
        legacy.endpoints.models = None;
        let legacy_fingerprint = legacy.routing_fingerprint().unwrap();
        assert!(
            resolve_provider(&document.providers[0])
                .matches_cache_fingerprint(&document.providers[0], &legacy_fingerprint)
                .unwrap()
        );
    }
}
