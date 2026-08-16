use std::collections::{BTreeMap, BTreeSet};

use provider_x_core::{
    CatalogModelId, DiscoveredModel, MetadataSource, ModelId, ModelPublicationStatus,
    ProviderConfig, ProviderModelCache, ProviderModelSource, ProviderModelSpec,
};

use crate::CatalogError;

const DISPLAY_NAME: &str = "display_name";
const CONTEXT_WINDOW: &str = "context_window";
const REASONING_LEVELS: &str = "supported_reasoning_levels";
const PARALLEL_TOOLS: &str = "supports_parallel_tool_calls";
const SEARCH_TOOL: &str = "supports_search_tool";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshPreview {
    pub cache: ProviderModelCache,
    pub added: Vec<ModelId>,
    pub removed: Vec<ModelId>,
    pub needs_review: Vec<ModelId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCapabilityConfirmation {
    pub display_name: String,
    pub context_window: u64,
    pub supported_reasoning_levels: Vec<String>,
    pub supports_parallel_tool_calls: bool,
    pub supports_search_tool: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCapabilitySettings {
    pub display_name: String,
    pub context_window: Option<u64>,
    pub supported_reasoning_levels: Vec<String>,
    pub supports_parallel_tool_calls: bool,
    pub supports_search_tool: bool,
}

/// Updates one model's editable parameters without changing whether the model is enabled.
///
/// # Errors
///
/// Returns an error when the model is absent or its display name is empty.
pub fn update_model_capabilities(
    preview: &mut RefreshPreview,
    model_id: &ModelId,
    settings: ModelCapabilitySettings,
) -> Result<(), CatalogError> {
    let display_name = settings.display_name.trim().to_owned();
    if display_name.is_empty() {
        return Err(CatalogError::EmptyModelDisplayName);
    }

    let mut seen_reasoning_levels = BTreeSet::new();
    let reasoning_levels = settings
        .supported_reasoning_levels
        .into_iter()
        .map(|level| level.trim().to_owned())
        .filter(|level| !level.is_empty())
        .filter(|level| seen_reasoning_levels.insert(level.clone()))
        .collect::<Vec<_>>();

    let model = preview
        .cache
        .models
        .iter_mut()
        .find(|model| &model.upstream_model_id == model_id)
        .ok_or_else(|| CatalogError::PreviewModelNotFound(model_id.to_string()))?;
    model.display_name = display_name;
    model.context_window = settings.context_window;
    model.supported_reasoning_levels = reasoning_levels;
    model.supports_parallel_tool_calls = Some(settings.supports_parallel_tool_calls);
    model.supports_search_tool = Some(settings.supports_search_tool);
    for field in [
        DISPLAY_NAME,
        CONTEXT_WINDOW,
        REASONING_LEVELS,
        PARALLEL_TOOLS,
        SEARCH_TOOL,
    ] {
        model
            .metadata_sources
            .insert(field.to_owned(), MetadataSource::UserConfirmed);
    }
    Ok(())
}

/// Enables or disables one model in a staged refresh preview.
///
/// This is an explicit user choice and does not require capability metadata to be complete.
///
/// # Errors
///
/// Returns an error when the model is absent.
pub fn set_model_enabled(
    preview: &mut RefreshPreview,
    model_id: &ModelId,
    enabled: bool,
) -> Result<(), CatalogError> {
    let model = preview
        .cache
        .models
        .iter_mut()
        .find(|model| &model.upstream_model_id == model_id)
        .ok_or_else(|| CatalogError::PreviewModelNotFound(model_id.to_string()))?;
    model.publication_status = if enabled {
        ModelPublicationStatus::Ready
    } else {
        ModelPublicationStatus::NeedsReview
    };
    Ok(())
}

/// Applies explicitly reviewed capabilities to one model in a refresh preview.
///
/// Every supplied capability becomes user-confirmed metadata and therefore survives later manual
/// refreshes when the Provider omits that field.
///
/// # Errors
///
/// Returns an error when the model is absent, its display name is empty, or its context window is
/// zero.
pub fn confirm_model_capabilities(
    preview: &mut RefreshPreview,
    model_id: &ModelId,
    confirmation: ModelCapabilityConfirmation,
) -> Result<(), CatalogError> {
    let display_name = confirmation.display_name.trim().to_owned();
    if display_name.is_empty() {
        return Err(CatalogError::EmptyModelDisplayName);
    }
    if confirmation.context_window == 0 {
        return Err(CatalogError::InvalidContextWindow);
    }

    let mut seen_reasoning_levels = BTreeSet::new();
    let reasoning_levels = confirmation
        .supported_reasoning_levels
        .into_iter()
        .map(|level| level.trim().to_owned())
        .filter(|level| !level.is_empty())
        .filter(|level| seen_reasoning_levels.insert(level.clone()))
        .collect::<Vec<_>>();

    let model = preview
        .cache
        .models
        .iter_mut()
        .find(|model| &model.upstream_model_id == model_id)
        .ok_or_else(|| CatalogError::PreviewModelNotFound(model_id.to_string()))?;
    model.display_name = display_name;
    model.context_window = Some(confirmation.context_window);
    model.supported_reasoning_levels = reasoning_levels;
    model.supports_parallel_tool_calls = Some(confirmation.supports_parallel_tool_calls);
    model.supports_search_tool = Some(confirmation.supports_search_tool);
    model.publication_status = ModelPublicationStatus::Ready;
    for field in [
        DISPLAY_NAME,
        CONTEXT_WINDOW,
        REASONING_LEVELS,
        PARALLEL_TOOLS,
        SEARCH_TOOL,
    ] {
        model
            .metadata_sources
            .insert(field.to_owned(), MetadataSource::UserConfirmed);
    }
    preview.needs_review.retain(|pending| pending != model_id);
    Ok(())
}

/// Builds a last-known-good cache candidate from one explicit Provider discovery response.
///
/// User-confirmed values survive refresh. Provider-derived values are replaced by what the current
/// response explicitly reports; absent capabilities remain absent rather than receiving defaults.
///
/// # Errors
///
/// Returns an error for an invalid Provider, invalid model identifier, or fingerprint failure.
pub fn build_refresh_preview(
    provider: &ProviderConfig,
    discovered: Vec<DiscoveredModel>,
    existing: Option<&ProviderModelCache>,
    refreshed_at: impl Into<String>,
) -> Result<RefreshPreview, CatalogError> {
    provider.validate()?;
    let previous = existing
        .map(|cache| {
            cache
                .models
                .iter()
                .map(|model| (model.upstream_model_id.clone(), model))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();

    let old_ids = previous.keys().cloned().collect::<BTreeSet<_>>();
    let mut new_ids = BTreeSet::new();
    let mut models = Vec::with_capacity(discovered.len());
    for model in discovered {
        let upstream_model_id = ModelId::new(&model.id)?;
        if !new_ids.insert(upstream_model_id.clone()) {
            continue;
        }
        let old = previous.get(&upstream_model_id).copied();
        models.push(merge_model(provider, upstream_model_id, model, old));
    }
    models.sort_by(|left, right| left.upstream_model_id.cmp(&right.upstream_model_id));

    let added = new_ids.difference(&old_ids).cloned().collect();
    let removed = old_ids.difference(&new_ids).cloned().collect();
    let needs_review = models
        .iter()
        .filter(|model| model.publication_status == ModelPublicationStatus::NeedsReview)
        .map(|model| model.upstream_model_id.clone())
        .collect();
    let cache = ProviderModelCache {
        config_fingerprint: provider.routing_fingerprint()?,
        last_successful_refresh_at: refreshed_at.into(),
        source: ProviderModelSource {
            protocol: provider.protocol,
            endpoint: provider.endpoints.models.clone().unwrap_or_else(|| {
                match provider.protocol {
                    provider_x_core::ProtocolId::OpenaiResponses => {
                        protocol_openai_responses::model_list_url(&provider.endpoints.http)
                    }
                    provider_x_core::ProtocolId::OpenaiChatCompletions => {
                        protocol_openai_chat_completions::model_list_url(&provider.endpoints.http)
                    }
                    provider_x_core::ProtocolId::AnthropicMessages => {
                        protocol_anthropic_messages::model_list_url(&provider.endpoints.http)
                    }
                }
            }),
        },
        models,
    };

    Ok(RefreshPreview {
        cache,
        added,
        removed,
        needs_review,
    })
}

fn merge_model(
    provider: &ProviderConfig,
    upstream_model_id: ModelId,
    discovered: DiscoveredModel,
    old: Option<&ProviderModelSpec>,
) -> ProviderModelSpec {
    let mut sources = BTreeMap::new();

    let display_name = choose_string(DISPLAY_NAME, discovered.display_name, old, &mut sources)
        .unwrap_or_else(|| upstream_model_id.to_string());
    let context_window = choose_copy(
        CONTEXT_WINDOW,
        discovered.context_window,
        old.and_then(|model| model.context_window),
        old,
        &mut sources,
    );
    let supported_reasoning_levels = choose_vec(
        REASONING_LEVELS,
        discovered.supported_reasoning_levels,
        old,
        &mut sources,
    )
    .unwrap_or_default();
    let supports_parallel_tool_calls = choose_copy(
        PARALLEL_TOOLS,
        discovered.supports_parallel_tool_calls,
        old.and_then(|model| model.supports_parallel_tool_calls),
        old,
        &mut sources,
    );
    let supports_search_tool = choose_copy(
        SEARCH_TOOL,
        discovered.supports_search_tool,
        old.and_then(|model| model.supports_search_tool),
        old,
        &mut sources,
    );

    ProviderModelSpec {
        catalog_model_id: CatalogModelId::for_provider(&provider.id, &upstream_model_id),
        upstream_model_id,
        display_name,
        publication_status: old.map_or(ModelPublicationStatus::NeedsReview, |model| {
            model.publication_status
        }),
        context_window,
        supported_reasoning_levels,
        supports_parallel_tool_calls,
        supports_search_tool,
        metadata_sources: sources,
    }
}

fn is_user_confirmed(old: Option<&ProviderModelSpec>, field: &str) -> bool {
    old.and_then(|model| model.metadata_sources.get(field)) == Some(&MetadataSource::UserConfirmed)
}

fn choose_string(
    field: &str,
    discovered: Option<String>,
    old: Option<&ProviderModelSpec>,
    sources: &mut BTreeMap<String, MetadataSource>,
) -> Option<String> {
    if is_user_confirmed(old, field) {
        sources.insert(field.to_owned(), MetadataSource::UserConfirmed);
        return old.map(|model| model.display_name.clone());
    }
    if discovered.is_some() {
        sources.insert(field.to_owned(), MetadataSource::ProviderModels);
    }
    discovered
}

fn choose_vec(
    field: &str,
    discovered: Option<Vec<String>>,
    old: Option<&ProviderModelSpec>,
    sources: &mut BTreeMap<String, MetadataSource>,
) -> Option<Vec<String>> {
    if is_user_confirmed(old, field) {
        sources.insert(field.to_owned(), MetadataSource::UserConfirmed);
        return old.map(|model| model.supported_reasoning_levels.clone());
    }
    if discovered.is_some() {
        sources.insert(field.to_owned(), MetadataSource::ProviderModels);
    }
    discovered
}

fn choose_copy<T: Copy>(
    field: &str,
    discovered: Option<T>,
    previous: Option<T>,
    old: Option<&ProviderModelSpec>,
    sources: &mut BTreeMap<String, MetadataSource>,
) -> Option<T> {
    if is_user_confirmed(old, field) {
        sources.insert(field.to_owned(), MetadataSource::UserConfirmed);
        return previous;
    }
    if discovered.is_some() {
        sources.insert(field.to_owned(), MetadataSource::ProviderModels);
    }
    discovered
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use provider_x_core::{
        AuthConfig, DiscoveredModel, EndpointConfig, MetadataSource, ModelId,
        ModelPublicationStatus, ProtocolId, ProviderConfig, ProviderId, ProviderModelCache,
        ProviderModelSource, ProviderModelSpec, TransportConfig,
    };

    use super::{
        ModelCapabilityConfirmation, build_refresh_preview, confirm_model_capabilities,
        set_model_enabled,
    };

    fn provider() -> ProviderConfig {
        ProviderConfig {
            id: ProviderId::new("provider-a").unwrap(),
            name: "Provider A".to_owned(),
            description: None,
            enabled: false,
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
        }
    }

    #[test]
    fn newly_discovered_models_default_to_disabled() {
        let preview = build_refresh_preview(
            &provider(),
            vec![DiscoveredModel {
                id: "coder".to_owned(),
                display_name: Some("Coder".to_owned()),
                context_window: Some(128_000),
                supported_reasoning_levels: None,
                supports_parallel_tool_calls: None,
                supports_search_tool: None,
            }],
            None,
            "2026-08-12T12:00:00Z",
        )
        .unwrap();

        let model = &preview.cache.models[0];
        assert_eq!(
            model.publication_status,
            ModelPublicationStatus::NeedsReview
        );
        assert_eq!(model.supports_parallel_tool_calls, None);
        assert_eq!(model.supports_search_tool, None);
        assert_eq!(preview.added, vec![ModelId::new("coder").unwrap()]);
        assert_eq!(preview.needs_review, vec![ModelId::new("coder").unwrap()]);
    }

    #[test]
    fn refresh_preserves_each_existing_model_enable_state() {
        let provider = provider();
        let model_id = ModelId::new("coder").unwrap();
        let discovered = || {
            vec![DiscoveredModel {
                id: "coder".to_owned(),
                display_name: Some("Coder".to_owned()),
                context_window: None,
                supported_reasoning_levels: None,
                supports_parallel_tool_calls: None,
                supports_search_tool: None,
            }]
        };
        let mut initial = build_refresh_preview(&provider, discovered(), None, "initial").unwrap();

        set_model_enabled(&mut initial, &model_id, true).unwrap();
        let enabled =
            build_refresh_preview(&provider, discovered(), Some(&initial.cache), "enabled")
                .unwrap();
        assert_eq!(
            enabled.cache.models[0].publication_status,
            ModelPublicationStatus::Ready
        );

        let mut disabled = enabled;
        set_model_enabled(&mut disabled, &model_id, false).unwrap();
        let refreshed =
            build_refresh_preview(&provider, discovered(), Some(&disabled.cache), "disabled")
                .unwrap();
        assert_eq!(
            refreshed.cache.models[0].publication_status,
            ModelPublicationStatus::NeedsReview
        );
    }

    #[test]
    fn refresh_preserves_user_confirmed_capabilities_only() {
        let provider = provider();
        let model_id = ModelId::new("coder").unwrap();
        let mut metadata_sources = BTreeMap::new();
        metadata_sources.insert("context_window".to_owned(), MetadataSource::UserConfirmed);
        metadata_sources.insert(
            "supports_parallel_tool_calls".to_owned(),
            MetadataSource::ProviderModels,
        );
        let old = ProviderModelCache {
            config_fingerprint: provider.routing_fingerprint().unwrap(),
            last_successful_refresh_at: "old".to_owned(),
            source: ProviderModelSource {
                protocol: ProtocolId::OpenaiResponses,
                endpoint: "old".to_owned(),
            },
            models: vec![ProviderModelSpec {
                upstream_model_id: model_id.clone(),
                catalog_model_id: provider_x_core::CatalogModelId::for_provider(
                    &provider.id,
                    &model_id,
                ),
                display_name: "Old".to_owned(),
                publication_status: ModelPublicationStatus::Ready,
                context_window: Some(64_000),
                supported_reasoning_levels: vec!["low".to_owned()],
                supports_parallel_tool_calls: Some(false),
                supports_search_tool: Some(false),
                metadata_sources,
            }],
        };
        let preview = build_refresh_preview(
            &provider,
            vec![DiscoveredModel {
                id: "coder".to_owned(),
                display_name: Some("New".to_owned()),
                context_window: Some(128_000),
                supported_reasoning_levels: Some(vec!["high".to_owned()]),
                supports_parallel_tool_calls: Some(true),
                supports_search_tool: None,
            }],
            Some(&old),
            "new",
        )
        .unwrap();

        let model = &preview.cache.models[0];
        assert_eq!(model.context_window, Some(64_000));
        assert_eq!(model.supports_parallel_tool_calls, Some(true));
        assert_eq!(model.supports_search_tool, None);
        assert_eq!(model.publication_status, ModelPublicationStatus::Ready);
    }

    #[test]
    fn explicit_confirmation_marks_a_model_ready_and_survives_refresh() {
        let provider = provider();
        let mut preview = build_refresh_preview(
            &provider,
            vec![DiscoveredModel {
                id: "coder".to_owned(),
                display_name: None,
                context_window: None,
                supported_reasoning_levels: None,
                supports_parallel_tool_calls: None,
                supports_search_tool: None,
            }],
            None,
            "2026-08-12T00:00:00Z",
        )
        .unwrap();
        let model_id = ModelId::new("coder").unwrap();

        confirm_model_capabilities(
            &mut preview,
            &model_id,
            ModelCapabilityConfirmation {
                display_name: " Coder ".to_owned(),
                context_window: 128_000,
                supported_reasoning_levels: vec!["low".to_owned(), "high".to_owned()],
                supports_parallel_tool_calls: true,
                supports_search_tool: false,
            },
        )
        .unwrap();

        let model = &preview.cache.models[0];
        assert_eq!(model.publication_status, ModelPublicationStatus::Ready);
        assert_eq!(model.display_name, "Coder");
        assert!(preview.needs_review.is_empty());
        assert!(
            model
                .metadata_sources
                .values()
                .all(|source| *source == MetadataSource::UserConfirmed)
        );

        let refreshed = build_refresh_preview(
            &provider,
            vec![DiscoveredModel {
                id: "coder".to_owned(),
                display_name: None,
                context_window: None,
                supported_reasoning_levels: None,
                supports_parallel_tool_calls: None,
                supports_search_tool: None,
            }],
            Some(&preview.cache),
            "2026-08-12T01:00:00Z",
        )
        .unwrap();
        assert_eq!(refreshed.cache.models[0], *model);
        assert!(refreshed.needs_review.is_empty());
    }
}
