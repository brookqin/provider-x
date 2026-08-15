use std::collections::BTreeMap;

use provider_x_app::control_plane::{AppPaths, ControlPlane, ControlPlaneError};
use provider_x_catalog::RefreshPreview;
use provider_x_core::{
    AuthConfig, CatalogModelId, EndpointConfig, MetadataSource, ModelId, ModelPublicationStatus,
    ProtocolId, ProviderConfig, ProviderId, ProviderModelCache, ProviderModelSource,
    ProviderModelSpec, RouteDecision, RouteResolver, RuntimeSnapshot, TransportConfig,
};

fn resolve(control: &ControlPlane, model: &str) -> RouteDecision {
    RuntimeSnapshot::build(control.providers(), control.cache())
        .unwrap()
        .resolve(model)
}

fn provider() -> ProviderConfig {
    ProviderConfig {
        id: ProviderId::new("provider-a").unwrap(),
        name: "Provider A".to_owned(),
        description: None,
        enabled: false,
        protocol: ProtocolId::OpenaiResponses,
        endpoints: EndpointConfig {
            http: "https://gateway.example/v1".to_owned(),
            websocket: None,
        },
        auth: AuthConfig::Bearer {
            api_key: "provider-secret".to_owned(),
        },
        transports: TransportConfig {
            http_sse: true,
            websocket: false,
        },
    }
}

fn preview(provider: &ProviderConfig) -> RefreshPreview {
    let upstream = ModelId::new("coder").unwrap();
    RefreshPreview {
        cache: ProviderModelCache {
            config_fingerprint: provider.routing_fingerprint().unwrap(),
            last_successful_refresh_at: "2026-08-12T12:00:00Z".to_owned(),
            source: ProviderModelSource {
                protocol: provider.protocol,
                endpoint: "https://gateway.example/v1/models".to_owned(),
            },
            models: vec![ProviderModelSpec {
                upstream_model_id: upstream.clone(),
                catalog_model_id: CatalogModelId::for_provider(&provider.id, &upstream),
                display_name: "Coder".to_owned(),
                publication_status: ModelPublicationStatus::Ready,
                context_window: Some(128_000),
                supported_reasoning_levels: vec!["low".to_owned()],
                supports_parallel_tool_calls: Some(true),
                supports_search_tool: Some(false),
                metadata_sources: BTreeMap::from([(
                    "context_window".to_owned(),
                    MetadataSource::UserConfirmed,
                )]),
            }],
        },
        added: vec![upstream],
        removed: Vec::new(),
        needs_review: Vec::new(),
    }
}

#[test]
fn app_paths_use_the_bundle_identifier_directory() {
    let home = tempfile::tempdir().unwrap();
    let paths = AppPaths::for_home(home.path());

    assert_eq!(
        paths.root,
        home.path()
            .join("Library/Application Support/dev.qiankun.provider-x")
    );
    assert_eq!(paths.providers, paths.root.join("providers.yaml"));
    assert_eq!(paths.model_cache, paths.root.join("cache/models.yaml"));
    assert_eq!(paths.ui_locale, paths.root.join("ui-locale"));
    assert_eq!(paths.logs, paths.root.join("logs"));
}

#[test]
fn first_launch_is_read_only_until_refresh_is_committed() {
    let home = tempfile::tempdir().unwrap();
    let paths = AppPaths::for_home(home.path());
    let mut control = ControlPlane::load(&paths).unwrap();

    assert!(control.providers().providers.is_empty());
    assert!(!paths.root.exists());

    let provider = provider();
    let outcome = control
        .commit_refresh(provider.clone(), preview(&provider))
        .unwrap();
    assert_eq!(outcome.provider_id, provider.id);
    assert!(!control.providers().providers[0].enabled);
    assert!(paths.providers.is_file());
    assert!(paths.model_cache.is_file());
    assert_eq!(
        resolve(&control, "provider-a/coder"),
        RouteDecision::UnavailableManagedModel
    );

    control.set_provider_enabled(&provider.id, true).unwrap();
    assert!(control.providers().providers[0].enabled);
    assert!(matches!(
        resolve(&control, "provider-a/coder"),
        RouteDecision::ThirdParty { .. }
    ));
}

#[test]
fn refreshing_an_enabled_provider_preserves_enable_state() {
    let home = tempfile::tempdir().unwrap();
    let paths = AppPaths::for_home(home.path());
    let mut control = ControlPlane::load(&paths).unwrap();
    let provider = provider();
    control
        .commit_refresh(provider.clone(), preview(&provider))
        .unwrap();
    control.set_provider_enabled(&provider.id, true).unwrap();

    let mut chat = provider.clone();
    chat.enabled = true;
    chat.protocol = ProtocolId::OpenaiChatCompletions;
    let outcome = control
        .commit_refresh(chat.clone(), preview(&chat))
        .unwrap();

    assert_eq!(outcome.provider_id, provider.id);
    assert!(control.providers().providers[0].enabled);
    assert_eq!(
        control.providers().providers[0].protocol,
        ProtocolId::OpenaiChatCompletions
    );
    assert!(matches!(
        resolve(&control, "provider-a/coder"),
        RouteDecision::ThirdParty { .. }
    ));
}

#[test]
fn refreshing_a_disabled_provider_does_not_enable_it() {
    let home = tempfile::tempdir().unwrap();
    let paths = AppPaths::for_home(home.path());
    let mut control = ControlPlane::load(&paths).unwrap();
    let provider = provider();

    control
        .commit_refresh(provider.clone(), preview(&provider))
        .unwrap();

    assert!(!control.providers().providers[0].enabled);
    assert_eq!(
        resolve(&control, "provider-a/coder"),
        RouteDecision::UnavailableManagedModel
    );
}

#[test]
fn refresh_uses_the_draft_enable_state_without_reading_the_old_state() {
    let home = tempfile::tempdir().unwrap();
    let paths = AppPaths::for_home(home.path());
    let mut control = ControlPlane::load(&paths).unwrap();
    let provider = provider();
    control
        .commit_refresh(provider.clone(), preview(&provider))
        .unwrap();

    let mut enabled_draft = provider.clone();
    enabled_draft.enabled = true;
    control
        .commit_refresh(enabled_draft.clone(), preview(&enabled_draft))
        .unwrap();

    assert!(control.providers().providers[0].enabled);
}

#[test]
fn provider_can_be_enabled_without_model_capability_review() {
    let home = tempfile::tempdir().unwrap();
    let paths = AppPaths::for_home(home.path());
    let mut control = ControlPlane::load(&paths).unwrap();
    let provider = provider();
    let mut incomplete = preview(&provider);
    incomplete.cache.models[0].publication_status = ModelPublicationStatus::NeedsReview;
    incomplete.cache.models[0].supports_search_tool = None;
    incomplete.needs_review = incomplete.added.clone();
    control
        .commit_refresh(provider.clone(), incomplete)
        .unwrap();

    control
        .set_provider_enabled(&provider.id, true)
        .expect("provider enablement must not depend on model capability review");
    assert!(control.providers().providers[0].enabled);
}

#[test]
fn removing_provider_updates_routes_but_preserves_model_cache() {
    let home = tempfile::tempdir().unwrap();
    let paths = AppPaths::for_home(home.path());
    let provider = provider();
    let mut control = ControlPlane::load(&paths).expect("load control plane");
    let preview = preview(&provider);
    control
        .commit_refresh(provider.clone(), preview)
        .expect("commit provider");
    control
        .set_provider_enabled(&provider.id, true)
        .expect("enable provider");

    control
        .remove_provider(&provider.id)
        .expect("remove provider");

    assert!(control.providers().providers.is_empty());
    assert!(control.cache().providers.contains_key(&provider.id));
    assert_eq!(
        resolve(&control, "provider-a/coder"),
        RouteDecision::UnavailableManagedModel
    );
    let reloaded = ControlPlane::load(&paths).expect("reload control plane");
    assert!(reloaded.providers().providers.is_empty());
    assert!(reloaded.cache().providers.contains_key(&provider.id));
}

#[test]
fn unknown_provider_is_reported_before_model_readiness() {
    let home = tempfile::tempdir().unwrap();
    let paths = AppPaths::for_home(home.path());
    let mut control = ControlPlane::load(&paths).unwrap();
    let missing = ProviderId::new("missing").unwrap();

    let error = control.set_provider_enabled(&missing, true).unwrap_err();

    assert!(matches!(error, ControlPlaneError::ProviderNotFound(_)));
}
