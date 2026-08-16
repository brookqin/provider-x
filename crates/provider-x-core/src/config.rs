use std::{collections::BTreeSet, fmt};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{CoreError, ProtocolId, ProviderId};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenerConfig {
    pub host: String,
    pub port: u16,
    pub request_body_limit_bytes: u64,
    pub max_connections: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeoutConfig {
    pub request_body_ms: u64,
    pub connect_ms: u64,
    pub response_headers_ms: u64,
    pub stream_idle_ms: u64,
    pub websocket_idle_ms: u64,
    pub shutdown_grace_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexConfig {
    pub manage_user_config: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointConfig {
    pub http: String,
    pub websocket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnthropicThinkingMode {
    #[default]
    Adaptive,
    Enabled,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum AuthConfig {
    Bearer { api_key: String },
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bearer { .. } => formatter
                .debug_struct("Bearer")
                .field("api_key", &"[REDACTED]")
                .finish(),
        }
    }
}

impl AuthConfig {
    #[must_use]
    pub fn mode_name(&self) -> &'static str {
        match self {
            Self::Bearer { .. } => "bearer",
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Bearer { api_key } => api_key.is_empty(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportConfig {
    pub http_sse: bool,
    pub websocket: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: ProviderId,
    pub name: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub protocol: ProtocolId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_thinking: Option<AnthropicThinkingMode>,
    pub endpoints: EndpointConfig,
    pub auth: AuthConfig,
    pub transports: TransportConfig,
}

impl ProviderConfig {
    #[must_use]
    pub fn anthropic_thinking_mode(&self) -> AnthropicThinkingMode {
        self.anthropic_thinking.unwrap_or_default()
    }

    /// Validates credentials, endpoints, and declared transport capabilities.
    ///
    /// # Errors
    ///
    /// Returns an error when a required credential/endpoint is missing or malformed.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.auth.is_empty() {
            return Err(CoreError::EmptyApiKey {
                provider_id: self.id.to_string(),
            });
        }
        match &self.auth {
            AuthConfig::Bearer { api_key }
                if api_key.trim() != api_key || api_key.chars().any(char::is_control) =>
            {
                return Err(CoreError::InvalidApiKey {
                    provider_id: self.id.to_string(),
                });
            }
            AuthConfig::Bearer { .. } => {}
        }
        if !is_absolute_http_url(&self.endpoints.http) {
            return Err(CoreError::InvalidHttpEndpoint {
                provider_id: self.id.to_string(),
            });
        }
        if self
            .endpoints
            .models
            .as_deref()
            .is_some_and(|endpoint| !is_absolute_http_url(endpoint))
        {
            return Err(CoreError::InvalidModelListEndpoint {
                provider_id: self.id.to_string(),
            });
        }
        if self.transports.websocket && self.endpoints.websocket.is_none() {
            return Err(CoreError::MissingWebSocketEndpoint {
                provider_id: self.id.to_string(),
            });
        }
        if let Some(websocket) = &self.endpoints.websocket
            && !is_absolute_websocket_url(websocket)
        {
            return Err(CoreError::InvalidWebSocketEndpoint {
                provider_id: self.id.to_string(),
            });
        }
        if matches!(
            self.protocol,
            ProtocolId::OpenaiChatCompletions | ProtocolId::AnthropicMessages
        ) && (self.transports.websocket || self.endpoints.websocket.is_some())
        {
            return Err(CoreError::ProtocolWebSocketUnsupported {
                provider_id: self.id.to_string(),
                protocol: match self.protocol {
                    ProtocolId::OpenaiChatCompletions => "OpenAI Chat Completions",
                    ProtocolId::AnthropicMessages => "Anthropic Messages",
                    ProtocolId::OpenaiResponses => unreachable!(),
                },
            });
        }
        Ok(())
    }

    /// Computes the non-secret fingerprint used to bind a model cache to routing semantics.
    ///
    /// # Errors
    ///
    /// Returns an error if the fingerprint input cannot be serialized.
    pub fn routing_fingerprint(&self) -> Result<String, CoreError> {
        #[derive(Serialize)]
        struct FingerprintInput<'a> {
            protocol: ProtocolId,
            endpoints: &'a EndpointConfig,
            auth_mode: &'static str,
            transports: &'a TransportConfig,
        }

        let bytes = serde_json::to_vec(&FingerprintInput {
            protocol: self.protocol,
            endpoints: &self.endpoints,
            auth_mode: self.auth.mode_name(),
            transports: &self.transports,
        })
        .map_err(|error| CoreError::FingerprintSerialization(error.to_string()))?;
        Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvidersDocument {
    pub schema_version: u32,
    pub listener: ListenerConfig,
    pub timeouts: TimeoutConfig,
    pub codex: CodexConfig,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
}

impl ProvidersDocument {
    /// Parses and validates a provider configuration YAML document.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid YAML or a configuration that violates the v1 schema.
    pub fn from_yaml(yaml: &str) -> Result<Self, CoreError> {
        let document: Self = yaml_serde::from_str(yaml)
            .map_err(|error| CoreError::InvalidYaml(error.to_string()))?;
        document.validate()?;
        Ok(document)
    }

    /// Validates document version, loopback listener settings, and every Provider.
    ///
    /// # Errors
    ///
    /// Returns the first schema or Provider validation failure.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(CoreError::UnsupportedSchemaVersion {
                actual: self.schema_version,
                expected: SCHEMA_VERSION,
            });
        }
        if self.listener.host != "127.0.0.1" {
            return Err(CoreError::InvalidListenerHost(self.listener.host.clone()));
        }
        if self.listener.port == 0 {
            return Err(CoreError::InvalidListenerPort);
        }
        if self.listener.request_body_limit_bytes == 0 || self.listener.max_connections == 0 {
            return Err(CoreError::InvalidListenerLimits);
        }
        if [
            self.timeouts.request_body_ms,
            self.timeouts.connect_ms,
            self.timeouts.response_headers_ms,
            self.timeouts.stream_idle_ms,
            self.timeouts.websocket_idle_ms,
            self.timeouts.shutdown_grace_ms,
        ]
        .contains(&0)
        {
            return Err(CoreError::InvalidTimeout);
        }

        let mut provider_ids = BTreeSet::new();
        for provider in &self.providers {
            if !provider_ids.insert(provider.id.clone()) {
                return Err(CoreError::DuplicateProviderId(provider.id.to_string()));
            }
            provider.validate()?;
        }
        Ok(())
    }
}

fn is_absolute_http_url(value: &str) -> bool {
    let Some((scheme, remainder)) = value.split_once("://") else {
        return false;
    };
    matches!(scheme, "http" | "https") && !remainder.is_empty() && !remainder.starts_with('/')
}

fn is_absolute_websocket_url(value: &str) -> bool {
    let Some((scheme, remainder)) = value.split_once("://") else {
        return false;
    };
    matches!(scheme, "ws" | "wss") && !remainder.is_empty() && !remainder.starts_with('/')
}
