pub mod claude_code;
#[path = "openai_oauth/auth.rs"]
mod oauth;
mod presets;
use bytes::Bytes;
use hyper::{HeaderMap, header, header::HeaderValue};
pub use oauth::{OpenAiOAuthClient, OpenAiOAuthError, needs_refresh as openai_oauth_needs_refresh};
pub use presets::{CredentialKind, PresetConnection, ProviderPreset, presets};
use provider_x_core::{
    AnthropicThinkingMode, AuthConfig, DiscoveredModel, ModelCacheDocument, ProtocolId,
    ProviderConfig, ProviderModelSource, ProvidersDocument, RuntimeSnapshot, TransportConfig,
};
use provider_x_protocol::output::ToolIdentity;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use thiserror::Error;
pub const OPENAI_HTTP_ENDPOINT: &str = "https://api.openai.com/v1";
pub const OPENAI_MODELS_ENDPOINT: &str = "https://api.openai.com/v1/models";
pub const OPENAI_WEBSOCKET_ENDPOINT: &str = "wss://api.openai.com/v1/responses";
pub const OPENAI_MODELS_DEV_ID: &str = "openai";
pub const OPENAI_OAUTH_HTTP_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex";
pub const OPENAI_OAUTH_MODELS_ENDPOINT: &str =
    "https://chatgpt.com/backend-api/codex/models?client_version=0.7.0";
pub const OPENAI_OAUTH_WEBSOCKET_ENDPOINT: &str = "wss://chatgpt.com/backend-api/codex/responses";
pub const DEEPSEEK_HTTP_ENDPOINT: &str = "https://api.deepseek.com/v1";
pub const DEEPSEEK_MODELS_ENDPOINT: &str = "https://api.deepseek.com/v1/models";
pub const DEEPSEEK_MODELS_DEV_ID: &str = "deepseek";
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AuthStyle {
    None,
    ClaudeCode,
    Bearer,
    OpenAiOAuth,
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
pub enum ExecutionBackend {
    Http,
    ClaudeCode,
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

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedHttpRequest {
    pub target: HttpTarget,
    pub body: Bytes,
    pub response_adapter: HttpResponseAdapter,
}

impl std::fmt::Debug for PreparedHttpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedHttpRequest")
            .field("body", &"<redacted>")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderProfile {
    adapter: ProtocolAdapter,
    auth_style: AuthStyle,
    http_endpoint: String,
    websocket_endpoint: Option<String>,
    models_endpoint: Option<String>,
    transports: TransportConfig,
    models_dev_id: Option<&'static str>,
    anthropic_thinking: AnthropicThinkingMode,
    reasoning_policy: provider_x_core::ReasoningPolicy,
    credential_scope: Option<String>,
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

    #[error("provider preset or connection is unknown")]
    UnknownPresetConnection,

    #[error("provider credentials do not match its connection mode")]
    AuthenticationMismatch,
}

#[must_use]
pub fn resolve_provider(provider: &ProviderConfig) -> ProviderProfile {
    ProviderProfile {
        adapter: provider.protocol.into(),
        auth_style: match &provider.auth {
            AuthConfig::None => AuthStyle::None,
            AuthConfig::ClaudeCode => AuthStyle::ClaudeCode,
            AuthConfig::OpenAiOAuth { .. } => AuthStyle::OpenAiOAuth,
            AuthConfig::Bearer { .. } if provider.protocol == ProtocolId::AnthropicMessages => {
                AuthStyle::Anthropic
            }
            AuthConfig::Bearer { .. } => AuthStyle::Bearer,
        },
        http_endpoint: provider.endpoints.http.clone(),
        websocket_endpoint: provider.endpoints.websocket.clone(),
        models_endpoint: provider.endpoints.models.clone(),
        transports: provider.transports.clone(),
        models_dev_id: match provider.preset.as_str() {
            "openai" => Some("openai"),
            "anthropic" => Some("anthropic"),
            "deepseek" => Some("deepseek"),
            "kimi" => Some("moonshotai"),
            "qwen" => Some("alibaba"),
            "zai" => Some("zhipuai"),
            "minimax" => Some("minimax"),
            "xai" => Some("xai"),
            "openrouter" => Some("openrouter"),
            "opencode" => Some("opencode"),
            _ => None,
        },
        anthropic_thinking: provider.anthropic_thinking_mode(),
        reasoning_policy: provider.reasoning_policy,
        credential_scope: match &provider.auth {
            AuthConfig::OpenAiOAuth { account_id, .. } => {
                Some(hex::encode(Sha256::digest(account_id.as_bytes())))
            }
            _ => None,
        },
    }
}

/// Validates a compiled instance independently of its preset defaults.
/// # Errors
/// Returns a validation error for malformed configuration or mismatched execution credentials.
pub fn validate_provider(provider: &ProviderConfig) -> Result<(), ProviderError> {
    provider.validate()?;
    let family = presets()
        .into_iter()
        .find(|preset| preset.id == provider.preset)
        .ok_or(ProviderError::UnknownPresetConnection)?;
    let connection = family
        .connections
        .iter()
        .find(|item| item.id == provider.connection)
        .ok_or(ProviderError::UnknownPresetConnection)?;
    if !matches!(
        (connection.credentials, &provider.auth),
        (CredentialKind::ApiKey, AuthConfig::Bearer { .. })
            | (CredentialKind::OpenaiOAuth, AuthConfig::OpenAiOAuth { .. })
            | (CredentialKind::ClaudeCode, AuthConfig::ClaudeCode)
            | (CredentialKind::None, AuthConfig::None)
    ) {
        return Err(ProviderError::AuthenticationMismatch);
    }
    if matches!(provider.auth, AuthConfig::OpenAiOAuth { .. })
        && provider.protocol != ProtocolId::OpenaiResponses
    {
        return Err(ProviderError::InvalidAuthenticationHeader);
    }
    if matches!(provider.auth, AuthConfig::ClaudeCode)
        && provider.protocol != ProtocolId::AnthropicMessages
    {
        return Err(ProviderError::InvalidAuthenticationHeader);
    }
    Ok(())
}

/// Validates current provider configuration.
/// # Errors
/// Returns the first invalid provider.
pub fn validate_document(providers: &ProvidersDocument) -> Result<(), ProviderError> {
    providers.validate()?;
    for provider in &providers.providers {
        validate_provider(provider)?;
    }
    Ok(())
}

/// Matches current routing semantics only.
/// # Errors
/// Returns an error if fingerprint serialization fails.
pub fn cache_fingerprint_matches(
    provider: &ProviderConfig,
    candidate: &str,
) -> Result<bool, provider_x_core::CoreError> {
    resolve_provider(provider)
        .routing_fingerprint()
        .map(|fingerprint| fingerprint == candidate)
        .map_err(|error| provider_x_core::CoreError::FingerprintSerialization(error.to_string()))
}

/// Builds routing exclusively from saved model selections.
/// # Errors
/// Returns a provider or model validation failure.
pub fn build_runtime_snapshot(
    providers: &ProvidersDocument,
    cache: &ModelCacheDocument,
) -> Result<RuntimeSnapshot, ProviderError> {
    validate_document(providers)?;
    RuntimeSnapshot::build(providers, cache).map_err(Into::into)
}
impl ProviderProfile {
    #[must_use]
    pub fn execution_backend(&self) -> ExecutionBackend {
        if self.auth_style == AuthStyle::ClaudeCode {
            ExecutionBackend::ClaudeCode
        } else {
            ExecutionBackend::Http
        }
    }

    #[must_use]
    pub const fn protocol(&self) -> ProtocolId {
        self.adapter.protocol()
    }

    #[must_use]
    pub fn reasoning_policy(&self) -> provider_x_core::ReasoningPolicy {
        self.reasoning_policy
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
                    provider_x_protocol::responses::model_list_url(&self.http_endpoint)
                }
                ProtocolAdapter::OpenaiChatCompletions => {
                    provider_x_protocol::chat_completions::model_list_url(&self.http_endpoint)
                }
                ProtocolAdapter::AnthropicMessages => {
                    provider_x_protocol::anthropic_messages::model_list_url(&self.http_endpoint)
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
        self.parse_model_list_by_adapter(bytes)
    }

    fn parse_model_list_by_adapter(
        &self,
        bytes: &[u8],
    ) -> Result<Vec<DiscoveredModel>, ProviderError> {
        match self.adapter {
            ProtocolAdapter::OpenaiResponses => {
                provider_x_protocol::responses::parse_model_list(bytes)
                    .map_err(|error| ProviderError::ModelDiscovery(error.to_string()))
            }
            ProtocolAdapter::OpenaiChatCompletions => {
                provider_x_protocol::chat_completions::parse_model_list(bytes)
                    .map_err(|error| ProviderError::ModelDiscovery(error.to_string()))
            }
            ProtocolAdapter::AnthropicMessages => {
                provider_x_protocol::anthropic_messages::parse_model_list(bytes)
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
        self.apply_authentication_by_style(auth, headers)
    }

    fn apply_authentication_by_style(
        &self,
        auth: &AuthConfig,
        headers: &mut HeaderMap,
    ) -> Result<(), ProviderError> {
        match (self.auth_style, auth) {
            (AuthStyle::None, AuthConfig::None) => {}
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
            (
                AuthStyle::OpenAiOAuth,
                AuthConfig::OpenAiOAuth {
                    access_token,
                    account_id,
                    is_fedramp,
                    ..
                },
            ) => {
                let authorization = HeaderValue::from_str(&format!("Bearer {access_token}"))
                    .map_err(|_| ProviderError::InvalidAuthenticationHeader)?;
                let account_id = HeaderValue::from_str(account_id)
                    .map_err(|_| ProviderError::InvalidAuthenticationHeader)?;
                headers.insert(header::AUTHORIZATION, authorization);
                headers.insert("chatgpt-account-id", account_id);
                headers
                    .entry("version")
                    .or_insert(HeaderValue::from_static(env!("CARGO_PKG_VERSION")));
                if *is_fedramp {
                    headers.insert("x-openai-fedramp", HeaderValue::from_static("true"));
                }
            }
            _ => return Err(ProviderError::InvalidAuthenticationHeader),
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
        self.prepare_http_request_by_adapter(body, upstream_model, max_bytes)
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
                body: provider_x_protocol::responses::rewrite_http_model(body, upstream_model)
                    .map_err(|error| ProviderError::RequestConversion(error.to_string()))?,
                response_adapter: HttpResponseAdapter::Passthrough,
            }),
            ProtocolAdapter::OpenaiChatCompletions => {
                let request =
                    provider_x_protocol::chat_completions::prepare_http_request_with_policy(
                        body,
                        upstream_model,
                        max_bytes,
                        self.reasoning_policy,
                    )
                    .map_err(|error| ProviderError::RequestConversion(error.to_string()))?;
                Ok(PreparedHttpRequest {
                    target: HttpTarget::Exact(
                        provider_x_protocol::chat_completions::chat_completions_url(
                            &self.http_endpoint,
                        ),
                    ),
                    body: request.body,
                    response_adapter: HttpResponseAdapter::OpenaiChatCompletions(
                        request.tool_names,
                    ),
                })
            }
            ProtocolAdapter::AnthropicMessages => {
                let request = provider_x_protocol::anthropic_messages::prepare_http_request_with_thinking_mode(
                    body,
                    upstream_model,
                    max_bytes,
                    self.anthropic_thinking,
                )
                .map_err(|error| ProviderError::RequestConversion(error.to_string()))?;
                Ok(PreparedHttpRequest {
                    target: HttpTarget::Exact(
                        provider_x_protocol::anthropic_messages::messages_url(&self.http_endpoint),
                    ),
                    body: request.body,
                    response_adapter: HttpResponseAdapter::AnthropicMessages(request.tool_names),
                })
            }
        }
    }

    #[must_use]
    pub fn websocket_plan(&self) -> WebSocketPlan {
        self.websocket_plan_by_adapter()
    }

    #[must_use]
    pub fn websocket_http_url(&self) -> String {
        match self.adapter {
            ProtocolAdapter::OpenaiResponses => {
                provider_x_protocol::responses::responses_url(&self.http_endpoint)
            }
            ProtocolAdapter::OpenaiChatCompletions => {
                provider_x_protocol::chat_completions::chat_completions_url(&self.http_endpoint)
            }
            ProtocolAdapter::AnthropicMessages => {
                provider_x_protocol::anthropic_messages::messages_url(&self.http_endpoint)
            }
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
        self.rewrite_websocket_request_by_adapter(message, upstream_model)
    }

    fn rewrite_websocket_request_by_adapter(
        &self,
        message: &str,
        upstream_model: &str,
    ) -> Result<String, ProviderError> {
        if self.adapter != ProtocolAdapter::OpenaiResponses {
            return Err(ProviderError::WebSocketConversionUnavailable);
        }
        provider_x_protocol::responses::rewrite_ws_text(message, upstream_model)
            .map_err(|error| ProviderError::RequestConversion(error.to_string()))
    }

    /// Returns a fingerprint of effective routing behavior.
    ///
    /// # Errors
    ///
    /// Returns an error if semantic fingerprint input serialization fails.
    pub fn routing_fingerprint(&self) -> Result<String, ProviderError> {
        #[derive(Serialize)]
        struct FingerprintInput<'a> {
            adapter: ProtocolAdapter,
            auth_style: AuthStyle,
            http_endpoint: &'a str,
            websocket_endpoint: Option<&'a str>,
            models_endpoint: String,
            transports: &'a TransportConfig,
            models_dev_id: Option<&'static str>,
            anthropic_thinking: AnthropicThinkingMode,
            reasoning_policy: provider_x_core::ReasoningPolicy,
            credential_scope: Option<&'a str>,
        }

        let bytes = serde_json::to_vec(&FingerprintInput {
            adapter: self.adapter,
            auth_style: self.auth_style,
            http_endpoint: &self.http_endpoint,
            websocket_endpoint: self.websocket_endpoint.as_deref(),
            models_endpoint: self.model_list_url(),
            transports: &self.transports,
            models_dev_id: self.models_dev_id,
            anthropic_thinking: self.anthropic_thinking,
            reasoning_policy: self.reasoning_policy,
            credential_scope: self.credential_scope.as_deref(),
        })
        .map_err(|error| ProviderError::FingerprintSerialization(error.to_string()))?;
        Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_request_debug_never_contains_body_or_target() {
        let request = PreparedHttpRequest {
            target: HttpTarget::Exact("https://example.invalid/private?token=synthetic".into()),
            body: Bytes::from_static(b"private-prompt"),
            response_adapter: HttpResponseAdapter::Passthrough,
        };
        let debug = format!("{request:?}");
        assert!(!debug.contains("private"));
        assert!(!debug.contains("synthetic"));
        assert!(debug.contains("redacted"));
    }
}
