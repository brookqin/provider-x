use std::{collections::BTreeSet, fmt};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{CoreError, ProtocolId, ProviderId, ProviderKind};

pub const SCHEMA_VERSION: u32 = 2;

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
    Bearer {
        api_key: String,
    },
    #[serde(rename = "openai_oauth")]
    OpenAiOAuth {
        access_token: String,
        refresh_token: String,
        account_id: String,
        expires_at_unix: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        email: Option<String>,
        #[serde(default)]
        is_fedramp: bool,
    },
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bearer { .. } => formatter
                .debug_struct("Bearer")
                .field("api_key", &"[REDACTED]")
                .finish(),
            Self::OpenAiOAuth { .. } => formatter
                .debug_struct("OpenAiOAuth")
                .field("access_token", &"[REDACTED]")
                .field("refresh_token", &"[REDACTED]")
                .field("account_id", &"[REDACTED]")
                .field("email", &"[REDACTED]")
                .finish_non_exhaustive(),
        }
    }
}

impl AuthConfig {
    #[must_use]
    pub fn mode_name(&self) -> &'static str {
        match self {
            Self::Bearer { .. } => "bearer",
            Self::OpenAiOAuth { .. } => "openai_oauth",
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Bearer { api_key } => api_key.is_empty(),
            Self::OpenAiOAuth {
                access_token,
                refresh_token,
                account_id,
                ..
            } => access_token.is_empty() || refresh_token.is_empty() || account_id.is_empty(),
        }
    }

    #[must_use]
    pub fn openai_oauth_expires_at_unix(&self) -> Option<u64> {
        match self {
            Self::OpenAiOAuth {
                expires_at_unix, ..
            } => Some(*expires_at_unix),
            Self::Bearer { .. } => None,
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
    pub kind: ProviderKind,
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
            return Err(match &self.auth {
                AuthConfig::Bearer { .. } => CoreError::EmptyApiKey {
                    provider_id: self.id.to_string(),
                },
                AuthConfig::OpenAiOAuth { .. } => CoreError::IncompleteOAuthCredentials {
                    provider_id: self.id.to_string(),
                },
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
            AuthConfig::OpenAiOAuth {
                access_token,
                refresh_token,
                account_id,
                email,
                expires_at_unix,
                ..
            } => {
                let invalid = [
                    access_token.as_str(),
                    refresh_token.as_str(),
                    account_id.as_str(),
                ]
                .into_iter()
                .any(|value| value.trim() != value || value.chars().any(char::is_control))
                    || email.as_deref().is_some_and(|value| {
                        value.trim() != value || value.chars().any(char::is_control)
                    })
                    || *expires_at_unix == 0;
                if invalid {
                    return Err(CoreError::InvalidOAuthCredentials {
                        provider_id: self.id.to_string(),
                    });
                }
            }
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
    /// Version-one documents are upgraded in memory by assigning an explicit provider kind. A
    /// version-two document must always state its kind, so `custom` is never confused with an
    /// absent legacy field.
    ///
    /// Returns an error for invalid YAML or a configuration that violates the current schema.
    pub fn from_yaml(yaml: &str) -> Result<Self, CoreError> {
        let mut value: yaml_serde::Value = yaml_serde::from_str(yaml)
            .map_err(|error| CoreError::InvalidYaml(error.to_string()))?;
        if schema_version(&value) == Some(1) {
            migrate_v1_document(&mut value)?;
        }
        let document: Self = yaml_serde::from_value(value)
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

fn schema_version(value: &yaml_serde::Value) -> Option<u64> {
    value.as_mapping()?.get("schema_version")?.as_u64()
}

fn migrate_v1_document(value: &mut yaml_serde::Value) -> Result<(), CoreError> {
    let document = value
        .as_mapping_mut()
        .ok_or_else(|| CoreError::InvalidYaml("provider document must be a mapping".to_owned()))?;
    document.insert(
        yaml_serde::Value::String("schema_version".to_owned()),
        yaml_serde::Value::Number(SCHEMA_VERSION.into()),
    );
    let providers = document
        .get_mut("providers")
        .and_then(yaml_serde::Value::as_sequence_mut)
        .ok_or_else(|| CoreError::InvalidYaml("providers must be a sequence".to_owned()))?;
    for provider in providers {
        let mapping = provider
            .as_mapping_mut()
            .ok_or_else(|| CoreError::InvalidYaml("provider must be a mapping".to_owned()))?;
        let is_legacy_deepseek = mapping.get("id").and_then(yaml_serde::Value::as_str)
            == Some("deepseek")
            && mapping.get("protocol").and_then(yaml_serde::Value::as_str)
                == Some("openai_responses")
            && mapping
                .get("endpoints")
                .and_then(yaml_serde::Value::as_mapping)
                .and_then(|endpoints| endpoints.get("http"))
                .and_then(yaml_serde::Value::as_str)
                .is_some_and(|endpoint| {
                    endpoint.trim_end_matches('/') == "https://api.deepseek.com"
                })
            && mapping
                .get("endpoints")
                .and_then(yaml_serde::Value::as_mapping)
                .and_then(|endpoints| endpoints.get("websocket"))
                .is_none_or(yaml_serde::Value::is_null)
            && mapping
                .get("endpoints")
                .and_then(yaml_serde::Value::as_mapping)
                .and_then(|endpoints| endpoints.get("models"))
                .is_none_or(|models| {
                    models.is_null() || models.as_str() == Some("https://api.deepseek.com/models")
                })
            && mapping
                .get("transports")
                .and_then(yaml_serde::Value::as_mapping)
                .is_some_and(|transports| {
                    transports
                        .get("http_sse")
                        .and_then(yaml_serde::Value::as_bool)
                        == Some(true)
                        && transports
                            .get("websocket")
                            .and_then(yaml_serde::Value::as_bool)
                            == Some(false)
                });
        if is_legacy_deepseek
            && let Some(endpoints) = mapping
                .get_mut("endpoints")
                .and_then(yaml_serde::Value::as_mapping_mut)
        {
            endpoints.insert(
                yaml_serde::Value::String("models".to_owned()),
                yaml_serde::Value::String("https://api.deepseek.com/models".to_owned()),
            );
        }
        mapping.insert(
            yaml_serde::Value::String("kind".to_owned()),
            yaml_serde::Value::String(
                if is_legacy_deepseek {
                    "deepseek"
                } else {
                    "custom"
                }
                .to_owned(),
            ),
        );
    }
    Ok(())
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
