use std::{collections::BTreeMap, fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::CoreError;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderId(String);

impl ProviderId {
    /// Creates a stable Provider identifier suitable for a catalog namespace.
    ///
    /// # Errors
    ///
    /// Returns an error unless the value is non-empty and contains only lowercase ASCII letters,
    /// digits, or `-`.
    pub fn new(value: impl Into<String>) -> Result<Self, CoreError> {
        let value = value.into();
        if value.is_empty()
            || !value.bytes().any(|byte| byte.is_ascii_alphanumeric())
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(CoreError::InvalidProviderId(value));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ProviderId {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ProviderId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProviderId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModelId(String);

impl ModelId {
    /// Creates an upstream model identifier while preserving its exact spelling.
    ///
    /// # Errors
    ///
    /// Returns an error for empty values, surrounding whitespace, or control characters.
    pub fn new(value: impl Into<String>) -> Result<Self, CoreError> {
        let value = value.into();
        if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
            return Err(CoreError::InvalidModelId(value));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ModelId {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ModelId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ModelId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CatalogModelId(String);

impl CatalogModelId {
    #[must_use]
    pub fn for_provider(provider_id: &ProviderId, upstream_model_id: &ModelId) -> Self {
        Self(format!("{provider_id}/{upstream_model_id}"))
    }

    /// Parses a `<provider-id>/<upstream-model-id>` catalog identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when either component violates its identity rules.
    pub fn parse(value: impl Into<String>) -> Result<Self, CoreError> {
        let value = value.into();
        let Some((provider, upstream)) = value.split_once('/') else {
            return Err(CoreError::InvalidModelId(value));
        };
        let provider_id = ProviderId::new(provider)?;
        let upstream_model_id = ModelId::new(upstream)?;
        Ok(Self::for_provider(&provider_id, &upstream_model_id))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CatalogModelId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for CatalogModelId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CatalogModelId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolId {
    #[default]
    OpenaiResponses,
    OpenaiChatCompletions,
    AnthropicMessages,
}

/// Protocol-neutral result of a Provider's explicit model-list request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub id: String,
    pub display_name: Option<String>,
    pub context_window: Option<u64>,
    pub supported_reasoning_levels: Option<Vec<String>>,
    pub supports_parallel_tool_calls: Option<bool>,
    pub supports_search_tool: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPublicationStatus {
    NeedsReview,
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataSource {
    ProviderModels,
    ModelRegistry,
    UserConfirmed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModelSpec {
    pub upstream_model_id: ModelId,
    pub catalog_model_id: CatalogModelId,
    pub display_name: String,
    pub publication_status: ModelPublicationStatus,
    pub context_window: Option<u64>,
    #[serde(default)]
    pub supported_reasoning_levels: Vec<String>,
    pub supports_parallel_tool_calls: Option<bool>,
    pub supports_search_tool: Option<bool>,
    #[serde(default)]
    pub metadata_sources: BTreeMap<String, MetadataSource>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModelSource {
    pub protocol: ProtocolId,
    pub endpoint: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModelCache {
    pub config_fingerprint: String,
    pub last_successful_refresh_at: String,
    pub source: ProviderModelSource,
    #[serde(default)]
    pub models: Vec<ProviderModelSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCacheDocument {
    pub schema_version: u32,
    #[serde(default)]
    pub providers: BTreeMap<ProviderId, ProviderModelCache>,
}

impl ModelCacheDocument {
    /// Parses and intrinsically validates a model cache YAML document.
    ///
    /// # Errors
    ///
    /// Returns an error when the YAML cannot be deserialized into the v1 cache schema.
    pub fn from_yaml(yaml: &str) -> Result<Self, CoreError> {
        let document: Self = yaml_serde::from_str(yaml)
            .map_err(|error| CoreError::InvalidYaml(error.to_string()))?;
        document.validate()?;
        Ok(document)
    }

    /// Validates schema version, unique upstream models, and deterministic catalog identities.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported schema versions, duplicates, or catalog ID mismatches.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.schema_version != crate::config::SCHEMA_VERSION {
            return Err(CoreError::UnsupportedSchemaVersion {
                actual: self.schema_version,
                expected: crate::config::SCHEMA_VERSION,
            });
        }

        for (provider_id, provider_cache) in &self.providers {
            let mut upstream_ids = std::collections::BTreeSet::new();
            for model in &provider_cache.models {
                if !upstream_ids.insert(model.upstream_model_id.clone()) {
                    return Err(CoreError::DuplicateModel {
                        provider_id: provider_id.to_string(),
                        model_id: model.upstream_model_id.to_string(),
                    });
                }
                let expected = CatalogModelId::for_provider(provider_id, &model.upstream_model_id);
                if model.catalog_model_id != expected {
                    return Err(CoreError::CatalogModelIdMismatch {
                        expected: expected.to_string(),
                        actual: model.catalog_model_id.to_string(),
                    });
                }
            }
        }
        Ok(())
    }
}
