use std::collections::BTreeSet;

use provider_x_core::{MetadataSource, ModelId, ProviderConfig, ProviderModelSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{CatalogError, RefreshPreview};

pub const MODEL_REGISTRY_URL: &str = "https://models.dev/api.json";
pub const MODEL_REGISTRY_SCHEMA_VERSION: u32 = 1;

const DISPLAY_NAME: &str = "display_name";
const CONTEXT_WINDOW: &str = "context_window";
const REASONING_LEVELS: &str = "supported_reasoning_levels";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelRegistryCache {
    pub schema_version: u32,
    pub source_url: String,
    pub fetched_at: String,
    pub etag: Option<String>,
    pub payload: Value,
}

impl ModelRegistryCache {
    /// Validates the cache envelope without accepting another registry origin.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsupported schema, unexpected source URL, or non-object payload.
    pub fn validate(&self) -> Result<(), CatalogError> {
        if self.schema_version != MODEL_REGISTRY_SCHEMA_VERSION {
            return Err(CatalogError::InvalidModelRegistry(
                "unsupported cache schema version".to_owned(),
            ));
        }
        if self.source_url != MODEL_REGISTRY_URL {
            return Err(CatalogError::InvalidModelRegistry(
                "unexpected registry source URL".to_owned(),
            ));
        }
        if !self.payload.is_object() {
            return Err(CatalogError::InvalidModelRegistry(
                "registry payload must be an object".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RegistryEnrichment {
    pub replacement_cache: Option<ModelRegistryCache>,
    pub matched_models: Vec<ModelId>,
    pub warning: Option<String>,
}

pub(crate) fn apply_registry_suggestions(
    provider: &ProviderConfig,
    preview: &mut RefreshPreview,
    cache: &ModelRegistryCache,
) -> Result<Vec<ModelId>, CatalogError> {
    cache.validate()?;
    let Some(provider_entry) = cache.payload.get(provider.id.as_str()) else {
        return Ok(Vec::new());
    };
    if provider_entry
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| id != provider.id.as_str())
    {
        return Ok(Vec::new());
    }
    let Some(models) = provider_entry.get("models").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };

    let mut matched = Vec::new();
    for model in &mut preview.cache.models {
        let model_id = model.upstream_model_id.as_str();
        let Some(registry_model) = models.get(model_id).and_then(Value::as_object) else {
            continue;
        };
        if registry_model
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id != model_id)
        {
            continue;
        }
        if apply_model_suggestion(model, registry_model) {
            matched.push(model.upstream_model_id.clone());
        }
    }
    Ok(matched)
}

fn apply_model_suggestion(
    model: &mut ProviderModelSpec,
    registry: &serde_json::Map<String, Value>,
) -> bool {
    let mut changed = false;
    if !model.metadata_sources.contains_key(DISPLAY_NAME)
        && let Some(name) = registry.get("name").and_then(Value::as_str)
        && !name.trim().is_empty()
    {
        name.trim().clone_into(&mut model.display_name);
        mark_registry_source(model, DISPLAY_NAME);
        changed = true;
    }
    if model.context_window.is_none()
        && let Some(context) = registry
            .get("limit")
            .and_then(Value::as_object)
            .and_then(|limit| limit.get("context"))
            .and_then(Value::as_u64)
            .filter(|context| *context > 0)
    {
        model.context_window = Some(context);
        mark_registry_source(model, CONTEXT_WINDOW);
        changed = true;
    }
    if model.supported_reasoning_levels.is_empty() {
        let levels = registry_reasoning_levels(registry);
        if !levels.is_empty() {
            model.supported_reasoning_levels = levels;
            mark_registry_source(model, REASONING_LEVELS);
            changed = true;
        }
    }
    changed
}

fn registry_reasoning_levels(registry: &serde_json::Map<String, Value>) -> Vec<String> {
    let Some(options) = registry.get("reasoning_options").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut seen = BTreeSet::new();
    let mut levels = Vec::new();
    for option in options {
        let Some(option) = option.as_object() else {
            continue;
        };
        if option.get("type").and_then(Value::as_str) != Some("effort") {
            continue;
        }
        let Some(values) = option.get("values").and_then(Value::as_array) else {
            continue;
        };
        for value in values {
            if let Some(level) = value.as_str().map(str::trim)
                && !level.is_empty()
                && seen.insert(level.to_owned())
            {
                levels.push(level.to_owned());
            }
        }
    }
    levels
}

fn mark_registry_source(model: &mut ProviderModelSpec, field: &str) {
    model
        .metadata_sources
        .insert(field.to_owned(), MetadataSource::ModelRegistry);
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use provider_x_core::{
        AuthConfig, CatalogModelId, EndpointConfig, ModelPublicationStatus, ProtocolId,
        ProviderConfig, ProviderId, ProviderModelCache, ProviderModelSource, ProviderModelSpec,
        TransportConfig,
    };
    use serde_json::{Value, json};

    use super::{
        MODEL_REGISTRY_SCHEMA_VERSION, MODEL_REGISTRY_URL, ModelRegistryCache,
        apply_registry_suggestions,
    };
    use crate::RefreshPreview;

    fn provider(id: &str) -> ProviderConfig {
        ProviderConfig {
            id: ProviderId::new(id).unwrap(),
            name: "Provider".to_owned(),
            description: None,
            enabled: false,
            protocol: ProtocolId::OpenaiResponses,
            endpoints: EndpointConfig {
                http: "https://gateway.example/v1".to_owned(),
                websocket: None,
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

    fn preview(provider: &ProviderConfig) -> RefreshPreview {
        let model_id = provider_x_core::ModelId::new("coder").unwrap();
        RefreshPreview {
            cache: ProviderModelCache {
                config_fingerprint: provider.routing_fingerprint().unwrap(),
                last_successful_refresh_at: "now".to_owned(),
                source: ProviderModelSource {
                    protocol: ProtocolId::OpenaiResponses,
                    endpoint: "https://gateway.example/v1/models".to_owned(),
                },
                models: vec![ProviderModelSpec {
                    upstream_model_id: model_id.clone(),
                    catalog_model_id: CatalogModelId::for_provider(&provider.id, &model_id),
                    display_name: "coder".to_owned(),
                    publication_status: ModelPublicationStatus::NeedsReview,
                    context_window: None,
                    supported_reasoning_levels: Vec::new(),
                    supports_parallel_tool_calls: None,
                    supports_search_tool: None,
                    metadata_sources: BTreeMap::new(),
                }],
            },
            added: vec![model_id.clone()],
            removed: Vec::new(),
            needs_review: vec![model_id],
        }
    }

    fn cache(payload: Value) -> ModelRegistryCache {
        ModelRegistryCache {
            schema_version: MODEL_REGISTRY_SCHEMA_VERSION,
            source_url: MODEL_REGISTRY_URL.to_owned(),
            fetched_at: "now".to_owned(),
            etag: Some("etag".to_owned()),
            payload,
        }
    }

    #[test]
    fn exact_provider_and_model_match_only_prefills_missing_fields() {
        let provider = provider("provider-a");
        let mut preview = preview(&provider);
        let matched = apply_registry_suggestions(
            &provider,
            &mut preview,
            &cache(json!({
                "provider-a": {
                    "id": "provider-a",
                    "models": {
                        "coder": {
                            "id": "coder",
                            "name": "Coder Suggested",
                            "limit": {"context": 128_000},
                            "reasoning_options": [{"type":"effort","values":["low","high"]}],
                            "tool_call": true
                        }
                    }
                }
            })),
        )
        .unwrap();

        assert_eq!(
            matched,
            vec![provider_x_core::ModelId::new("coder").unwrap()]
        );
        let model = &preview.cache.models[0];
        assert_eq!(model.display_name, "Coder Suggested");
        assert_eq!(model.context_window, Some(128_000));
        assert_eq!(model.supported_reasoning_levels, ["low", "high"]);
        assert_eq!(model.supports_parallel_tool_calls, None);
        assert_eq!(
            model.publication_status,
            ModelPublicationStatus::NeedsReview
        );
        assert!(
            model
                .metadata_sources
                .values()
                .all(|source| *source == provider_x_core::MetadataSource::ModelRegistry)
        );
    }

    #[test]
    fn fuzzy_provider_or_model_identity_never_matches() {
        let provider = provider("provider-a");
        for payload in [
            json!({"provider-a-alt":{"models":{"coder":{"limit":{"context":128_000}}}}}),
            json!({"provider-a":{"models":{"coder-high":{"limit":{"context":128_000}}}}}),
            json!({"provider-a":{"id":"other","models":{"coder":{"limit":{"context":128_000}}}}}),
            json!({"provider-a":{"models":{"coder":{"id":"other","limit":{"context":128_000}}}}}),
        ] {
            let mut preview = preview(&provider);
            assert!(
                apply_registry_suggestions(&provider, &mut preview, &cache(payload))
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(preview.cache.models[0].context_window, None);
        }
    }
}
