use std::collections::BTreeSet;

use provider_x_core::{ModelCacheDocument, ModelPublicationStatus, ProvidersDocument};
use provider_x_providers::{resolve_provider, validate_document};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::CatalogError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledCatalog {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub official_models: usize,
    pub third_party_models: usize,
}

/// Precompiled third-party catalog entries that contain no Provider credentials.
#[derive(Clone, Debug)]
pub struct CatalogOverlay {
    third_party: Vec<(String, Value)>,
}

impl CatalogOverlay {
    /// Builds the publishable namespaced model projection for enabled Providers.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid configuration, missing/stale cache, or duplicate private
    /// catalog IDs.
    pub fn from_documents(
        providers: &ProvidersDocument,
        cache: &ModelCacheDocument,
    ) -> Result<Self, CatalogError> {
        validate_document(providers)?;
        cache.validate()?;
        let mut slugs = BTreeSet::new();
        let mut third_party = Vec::new();
        for provider in providers
            .providers
            .iter()
            .filter(|provider| provider.enabled)
        {
            let provider_cache = cache.providers.get(&provider.id).ok_or_else(|| {
                provider_x_core::CoreError::MissingModelCache {
                    provider_id: provider.id.to_string(),
                }
            })?;
            if !resolve_provider(provider)
                .matches_cache_fingerprint(provider, &provider_cache.config_fingerprint)?
            {
                return Err(provider_x_core::CoreError::StaleModelCache {
                    provider_id: provider.id.to_string(),
                }
                .into());
            }
            for model in provider_cache
                .models
                .iter()
                .filter(|model| model.publication_status == ModelPublicationStatus::Ready)
            {
                let slug = model.catalog_model_id.to_string();
                if !slugs.insert(slug.clone()) {
                    return Err(CatalogError::InvalidCodexCatalog(format!(
                        "duplicate private catalog model slug {slug}"
                    )));
                }
                third_party.push((slug, render_model(provider.name.as_str(), model)));
            }
        }
        third_party.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(Self { third_party })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.third_party.is_empty()
    }

    /// Merges the precompiled private projection into one official Codex catalog response.
    ///
    /// Unknown official fields are preserved byte-for-byte semantically through `serde_json`.
    ///
    /// # Errors
    ///
    /// Returns an error when the official response is not a Codex catalog, a private slug
    /// conflicts with an official slug, or the merged document cannot be serialized.
    pub fn merge(&self, official: &[u8]) -> Result<CompiledCatalog, CatalogError> {
        let mut document: Value = serde_json::from_slice(official)
            .map_err(|error| CatalogError::InvalidCodexCatalog(error.to_string()))?;
        let models = document
            .as_object_mut()
            .and_then(|object| object.get_mut("models"))
            .and_then(Value::as_array_mut)
            .ok_or_else(|| {
                CatalogError::InvalidCodexCatalog("missing top-level models array".to_owned())
            })?;
        let official_models = models.len();
        let mut slugs = models
            .iter()
            .filter_map(|model| model.get("slug").and_then(Value::as_str))
            .map(ToOwned::to_owned)
            .collect::<BTreeSet<_>>();
        let personality_variables = personality_variables_from_official(models);

        for (slug, model) in &self.third_party {
            if !slugs.insert(slug.clone()) {
                return Err(CatalogError::InvalidCodexCatalog(format!(
                    "duplicate catalog model slug {slug}"
                )));
            }
            let mut model = model.clone();
            apply_personality_variables(&mut model, &personality_variables);
            models.push(model);
        }

        let mut bytes = serde_json::to_vec_pretty(&document)
            .map_err(|error| CatalogError::CatalogSerialization(error.to_string()))?;
        bytes.push(b'\n');
        Ok(CompiledCatalog {
            sha256: format!("sha256:{}", hex_digest(&bytes)),
            bytes,
            official_models,
            third_party_models: self.third_party.len(),
        })
    }
}

/// Merges an official Codex catalog with publishable namespaced Provider models.
///
/// # Errors
///
/// Returns an error when inputs are invalid, the bundled document has no model array, a cache is
/// stale, or serialization fails.
pub fn compile_catalog(
    official: &[u8],
    providers: &ProvidersDocument,
    cache: &ModelCacheDocument,
) -> Result<CompiledCatalog, CatalogError> {
    CatalogOverlay::from_documents(providers, cache)?.merge(official)
}

fn render_model(provider_name: &str, model: &provider_x_core::ProviderModelSpec) -> Value {
    let levels = model
        .supported_reasoning_levels
        .iter()
        .map(|effort| {
            json!({
                "effort": effort,
                "description": format!("Provider-declared {effort} reasoning")
            })
        })
        .collect::<Vec<_>>();
    let default_reasoning_level = model
        .supported_reasoning_levels
        .first()
        .map_or(Value::Null, |level| Value::String(level.clone()));
    let context_window = model.context_window.unwrap_or(128_000);
    let supports_parallel_tool_calls = model.supports_parallel_tool_calls.unwrap_or(false);
    let supports_search_tool = model.supports_search_tool.unwrap_or(false);
    let model_id = model.catalog_model_id.to_string();
    let instructions_template = format!(
        "You are {}. Your provider-x model identifier is {model_id}.\n\n{{{{ personality }}}}\n\nFollow the user's request and use the provided tools when needed.",
        model.display_name
    );
    let base_instructions = instructions_template.replace("{{ personality }}", "");
    json!({
        "slug": model_id,
        "display_name": format!("{provider_name} / {}", model.display_name),
        "description": format!("Routed through provider-x Provider {provider_name}"),
        "default_reasoning_level": default_reasoning_level,
        "supported_reasoning_levels": levels,
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "multi_agent_version": "v2",
        "priority": 100,
        "availability_nux": null,
        "upgrade": null,
        "base_instructions": base_instructions,
        "model_messages": {
            "instructions_template": instructions_template,
            "instructions_variables": {
                "personality_default": "",
                "personality_friendly": "",
                "personality_pragmatic": ""
            },
            "approvals": null,
            "collaboration_modes": null,
            "auto_review": null,
            "multi_agent": null,
            "permissions": null,
            "token_budget": null
        },
        "support_verbosity": false,
        "default_verbosity": null,
        "apply_patch_tool_type": null,
        "truncation_policy": {"mode": "tokens", "limit": 10000},
        "supports_parallel_tool_calls": supports_parallel_tool_calls,
        "context_window": context_window,
        "effective_context_window_percent": 95,
        "experimental_supported_tools": [],
        "input_modalities": ["text"],
        "supports_search_tool": supports_search_tool
    })
}

fn apply_personality_variables(model: &mut Value, personality_variables: &Value) {
    let personality_default = personality_variables
        .get("personality_default")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let rendered_instructions = model
        .pointer("/model_messages/instructions_template")
        .and_then(Value::as_str)
        .map(|template| template.replace("{{ personality }}", personality_default));

    model["model_messages"]["instructions_variables"] = personality_variables.clone();
    if let Some(rendered_instructions) = rendered_instructions {
        model["base_instructions"] = Value::String(rendered_instructions);
    }
}

fn personality_variables_from_official(models: &[Value]) -> Value {
    models
        .iter()
        .filter_map(|model| model.pointer("/model_messages/instructions_variables"))
        .find(|variables| {
            [
                "personality_default",
                "personality_friendly",
                "personality_pragmatic",
            ]
            .iter()
            .all(|key| variables.get(key).is_some_and(Value::is_string))
        })
        .cloned()
        .unwrap_or_else(|| {
            json!({
                "personality_default": "",
                "personality_friendly": "",
                "personality_pragmatic": ""
            })
        })
}

fn hex_digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use provider_x_core::{
        AuthConfig, CatalogModelId, CodexConfig, EndpointConfig, ListenerConfig, MetadataSource,
        ModelCacheDocument, ModelId, ModelPublicationStatus, ProtocolId, ProviderConfig,
        ProviderId, ProviderModelCache, ProviderModelSource, ProviderModelSpec, ProvidersDocument,
        TimeoutConfig, TransportConfig,
    };
    use serde_json::Value;

    use super::compile_catalog;

    fn inputs() -> (ProvidersDocument, ModelCacheDocument) {
        let provider_id = ProviderId::new("provider-a").unwrap();
        let provider = ProviderConfig {
            id: provider_id.clone(),
            name: "Provider A".to_owned(),
            description: None,
            enabled: true,
            kind: provider_x_core::ProviderKind::Custom,
            protocol: ProtocolId::OpenaiResponses,
            anthropic_thinking: None,
            endpoints: EndpointConfig {
                http: "https://gateway.example/v1".to_owned(),
                websocket: None,
                models: None,
            },
            auth: AuthConfig::Bearer {
                api_key: "secret".to_owned(),
            },
            transports: TransportConfig {
                http_sse: true,
                websocket: false,
            },
        };
        let model_id = ModelId::new("coder").unwrap();
        let cache = ProviderModelCache {
            config_fingerprint: provider_x_providers::resolve_provider(&provider)
                .routing_fingerprint()
                .unwrap(),
            last_successful_refresh_at: "2026-08-12T00:00:00Z".to_owned(),
            source: ProviderModelSource {
                protocol: ProtocolId::OpenaiResponses,
                endpoint: "https://gateway.example/v1/models".to_owned(),
            },
            models: vec![ProviderModelSpec {
                upstream_model_id: model_id.clone(),
                catalog_model_id: CatalogModelId::for_provider(&provider_id, &model_id),
                display_name: "Coder".to_owned(),
                publication_status: ModelPublicationStatus::Ready,
                context_window: Some(128_000),
                supported_reasoning_levels: vec!["low".to_owned(), "high".to_owned()],
                supports_parallel_tool_calls: Some(true),
                supports_search_tool: Some(false),
                metadata_sources: BTreeMap::from([(
                    "context_window".to_owned(),
                    MetadataSource::UserConfirmed,
                )]),
            }],
        };
        (
            ProvidersDocument {
                schema_version: provider_x_core::SCHEMA_VERSION,
                listener: ListenerConfig {
                    host: "127.0.0.1".to_owned(),
                    port: 43119,
                    request_body_limit_bytes: 1_000_000,
                    max_connections: 10,
                },
                timeouts: TimeoutConfig {
                    request_body_ms: 1,
                    connect_ms: 1,
                    response_headers_ms: 1,
                    stream_idle_ms: 1,
                    websocket_idle_ms: 1,
                    shutdown_grace_ms: 1,
                },
                codex: CodexConfig {
                    manage_user_config: false,
                },
                providers: vec![provider],
            },
            ModelCacheDocument {
                schema_version: 1,
                providers: BTreeMap::from([(provider_id, cache)]),
            },
        )
    }

    #[test]
    fn preserves_official_models_and_appends_namespaced_models() {
        let (providers, cache) = inputs();
        let bundled = br#"{"models":[{"slug":"official","opaque":{"kept":true},"model_messages":{"instructions_variables":{"personality_default":"default personality","personality_friendly":"friendly personality","personality_pragmatic":"pragmatic personality"}}}]}"#;
        let compiled = compile_catalog(bundled, &providers, &cache).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&compiled.bytes).unwrap();

        assert_eq!(compiled.official_models, 1);
        assert_eq!(compiled.third_party_models, 1);
        assert_eq!(value["models"][0]["opaque"]["kept"], true);
        assert_eq!(value["models"][1]["slug"], "provider-a/coder");
        assert_eq!(value["models"][1]["context_window"], 128_000);
        assert_eq!(value["models"][1]["multi_agent_version"], "v2");
        assert_eq!(
            value["models"][1]["base_instructions"],
            "You are Coder. Your provider-x model identifier is provider-a/coder.\n\ndefault personality\n\nFollow the user's request and use the provided tools when needed."
        );
        assert_eq!(
            value["models"][1]["model_messages"]["instructions_template"],
            "You are Coder. Your provider-x model identifier is provider-a/coder.\n\n{{ personality }}\n\nFollow the user's request and use the provided tools when needed."
        );
        assert_eq!(
            value["models"][1]["model_messages"]["instructions_variables"]["personality_friendly"],
            "friendly personality"
        );
        for field in [
            "approvals",
            "collaboration_modes",
            "auto_review",
            "multi_agent",
            "permissions",
            "token_budget",
        ] {
            assert_eq!(value["models"][1]["model_messages"][field], Value::Null);
        }
    }

    #[test]
    fn output_is_deterministic_and_rejects_stale_cache() {
        let (providers, mut cache) = inputs();
        let bundled = br#"{"models":[{"slug":"official"}]}"#;
        let first = compile_catalog(bundled, &providers, &cache).unwrap();
        let second = compile_catalog(bundled, &providers, &cache).unwrap();
        assert_eq!(first, second);
        let value: serde_json::Value = serde_json::from_slice(&first.bytes).unwrap();
        assert_eq!(
            value["models"][1]["model_messages"]["instructions_variables"]["personality_default"],
            ""
        );
        assert_eq!(
            value["models"][1]["base_instructions"],
            "You are Coder. Your provider-x model identifier is provider-a/coder.\n\n\n\nFollow the user's request and use the provided tools when needed."
        );
        assert!(
            value["models"][1]["model_messages"]["instructions_template"]
                .as_str()
                .unwrap()
                .contains("{{ personality }}")
        );

        cache
            .providers
            .values_mut()
            .next()
            .unwrap()
            .config_fingerprint = "sha256:stale".to_owned();
        let stale = compile_catalog(bundled, &providers, &cache).unwrap_err();
        assert!(stale.to_string().contains("fingerprint is stale"));
    }
}
