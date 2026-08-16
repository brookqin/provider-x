use std::collections::BTreeMap;

use provider_x_core::{
    AnthropicThinkingMode, AuthConfig, CatalogModelId, CodexConfig, EndpointConfig, ListenerConfig,
    ModelCacheDocument, ModelId, ModelPublicationStatus, ProtocolId, ProviderConfig, ProviderId,
    ProviderModelCache, ProviderModelSource, ProviderModelSpec, ProvidersDocument, RouteDecision,
    RouteResolver, RuntimeSnapshot, TimeoutConfig, TransportConfig,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

const PROVIDERS_YAML: &str = r"
schema_version: 1
listener:
  host: 127.0.0.1
  port: 43119
  request_body_limit_bytes: 33554432
  max_connections: 128
timeouts:
  request_body_ms: 30000
  connect_ms: 10000
  response_headers_ms: 30000
  stream_idle_ms: 300000
  websocket_idle_ms: 300000
  shutdown_grace_ms: 30000
codex:
  manage_user_config: true
providers:
  - id: compatible-primary
    name: Compatible Primary
    description: null
    enabled: false
    protocol: openai_responses
    endpoints:
      http: https://gateway.example.com/v1
      websocket: null
    auth:
      mode: bearer
      api_key: secret
    transports:
      http_sse: true
      websocket: false
";

fn provider(id: &str, enabled: bool) -> ProviderConfig {
    ProviderConfig {
        id: ProviderId::new(id).unwrap(),
        name: id.to_owned(),
        description: None,
        enabled,
        protocol: ProtocolId::OpenaiResponses,
        anthropic_thinking: None,
        endpoints: EndpointConfig {
            http: format!("https://{id}.example/v1"),
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

fn document(providers: Vec<ProviderConfig>) -> ProvidersDocument {
    ProvidersDocument {
        schema_version: 1,
        listener: ListenerConfig {
            host: "127.0.0.1".to_owned(),
            port: 43119,
            request_body_limit_bytes: 32 * 1024 * 1024,
            max_connections: 128,
        },
        timeouts: TimeoutConfig {
            request_body_ms: 30_000,
            connect_ms: 10_000,
            response_headers_ms: 30_000,
            stream_idle_ms: 300_000,
            websocket_idle_ms: 300_000,
            shutdown_grace_ms: 30_000,
        },
        codex: CodexConfig {
            manage_user_config: true,
        },
        providers,
    }
}

fn cached_provider(
    provider: &ProviderConfig,
    models: &[(&str, ModelPublicationStatus)],
) -> ProviderModelCache {
    ProviderModelCache {
        config_fingerprint: provider.routing_fingerprint().unwrap(),
        last_successful_refresh_at: "2026-08-11T10:00:00Z".to_owned(),
        source: ProviderModelSource {
            protocol: ProtocolId::OpenaiResponses,
            endpoint: format!("{}/models", provider.endpoints.http),
        },
        models: models
            .iter()
            .map(|(id, publication_status)| {
                let upstream_model_id = ModelId::new(*id).unwrap();
                ProviderModelSpec {
                    catalog_model_id: CatalogModelId::for_provider(
                        &provider.id,
                        &upstream_model_id,
                    ),
                    upstream_model_id,
                    display_name: (*id).to_owned(),
                    publication_status: *publication_status,
                    context_window: Some(128_000),
                    supported_reasoning_levels: vec!["low".to_owned()],
                    supports_parallel_tool_calls: Some(true),
                    supports_search_tool: Some(false),
                    metadata_sources: BTreeMap::new(),
                }
            })
            .collect(),
    }
}

#[test]
fn provider_namespace_allows_same_upstream_model_from_multiple_providers() {
    let first = provider("provider-a", true);
    let second = provider("provider-b", true);
    let mut cache = ModelCacheDocument {
        schema_version: 1,
        providers: BTreeMap::new(),
    };
    cache.providers.insert(
        first.id.clone(),
        cached_provider(&first, &[("coder/model", ModelPublicationStatus::Ready)]),
    );
    cache.providers.insert(
        second.id.clone(),
        cached_provider(&second, &[("coder/model", ModelPublicationStatus::Ready)]),
    );

    let snapshot = RuntimeSnapshot::build(&document(vec![first, second]), &cache).unwrap();
    assert_eq!(snapshot.published_model_count(), 2);
    assert!(matches!(
        snapshot.resolve("provider-a/coder/model"),
        RouteDecision::ThirdParty { provider_id, upstream_model_id }
            if provider_id.as_str() == "provider-a" && upstream_model_id.as_str() == "coder/model"
    ));
    assert!(matches!(
        snapshot.resolve("provider-b/coder/model"),
        RouteDecision::ThirdParty { provider_id, upstream_model_id }
            if provider_id.as_str() == "provider-b" && upstream_model_id.as_str() == "coder/model"
    ));
}

#[test]
fn bare_models_are_official_and_stale_namespaced_models_fail_closed() {
    let snapshot = RuntimeSnapshot::default();
    assert_eq!(snapshot.resolve("gpt-5.6"), RouteDecision::BuiltInOfficial);
    assert_eq!(
        snapshot.resolve("provider-a/coder"),
        RouteDecision::UnavailableManagedModel
    );
}

#[test]
fn needs_review_models_are_not_published() {
    let configured = provider("provider-a", true);
    let mut cache = ModelCacheDocument {
        schema_version: 1,
        providers: BTreeMap::new(),
    };
    cache.providers.insert(
        configured.id.clone(),
        cached_provider(
            &configured,
            &[("coder", ModelPublicationStatus::NeedsReview)],
        ),
    );

    let snapshot = RuntimeSnapshot::build(&document(vec![configured]), &cache).unwrap();
    assert_eq!(snapshot.published_model_count(), 0);
    assert_eq!(
        snapshot.resolve("provider-a/coder"),
        RouteDecision::UnavailableManagedModel
    );
}

#[test]
fn api_key_is_redacted_from_debug_output() {
    let configured = provider("provider-a", false);
    let debug = format!("{:?}", configured.auth);
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("secret"));
}

#[test]
fn fingerprint_does_not_change_when_api_key_rotates() {
    let first = provider("provider-a", false);
    let mut second = first.clone();
    second.auth = AuthConfig::Bearer {
        api_key: "different-secret".to_owned(),
    };
    assert_eq!(
        first.routing_fingerprint().unwrap(),
        second.routing_fingerprint().unwrap()
    );
}

#[test]
fn missing_model_endpoint_preserves_the_legacy_routing_fingerprint() {
    #[derive(Serialize)]
    struct LegacyEndpoint<'a> {
        http: &'a str,
        websocket: Option<&'a str>,
    }
    #[derive(Serialize)]
    struct LegacyFingerprint<'a> {
        protocol: ProtocolId,
        endpoints: LegacyEndpoint<'a>,
        auth_mode: &'static str,
        transports: &'a TransportConfig,
    }

    let configured = provider("provider-a", true);
    let legacy = serde_json::to_vec(&LegacyFingerprint {
        protocol: configured.protocol,
        endpoints: LegacyEndpoint {
            http: &configured.endpoints.http,
            websocket: configured.endpoints.websocket.as_deref(),
        },
        auth_mode: "bearer",
        transports: &configured.transports,
    })
    .unwrap();
    let legacy = format!("sha256:{}", hex::encode(Sha256::digest(legacy)));
    assert_eq!(configured.routing_fingerprint().unwrap(), legacy);
    let mut cache = ModelCacheDocument {
        schema_version: 1,
        providers: BTreeMap::from([(
            configured.id.clone(),
            cached_provider(&configured, &[("coder", ModelPublicationStatus::Ready)]),
        )]),
    };
    cache
        .providers
        .get_mut(&configured.id)
        .unwrap()
        .config_fingerprint
        .clone_from(&legacy);
    RuntimeSnapshot::build(&document(vec![configured.clone()]), &cache)
        .expect("legacy cache fingerprint must remain valid after upgrade");

    let mut overridden = configured;
    overridden.endpoints.models = Some("https://provider-a.example/models".to_owned());
    assert_ne!(overridden.routing_fingerprint().unwrap(), legacy);
}

#[test]
fn anthropic_thinking_defaults_to_adaptive_and_can_select_enabled() {
    let mut configured = provider("provider-a", false);
    configured.protocol = ProtocolId::AnthropicMessages;
    assert_eq!(
        configured.anthropic_thinking_mode(),
        AnthropicThinkingMode::Adaptive
    );
    configured.anthropic_thinking = Some(AnthropicThinkingMode::Enabled);
    assert_eq!(
        configured.anthropic_thinking_mode(),
        AnthropicThinkingMode::Enabled
    );
}

#[test]
fn provider_yaml_round_trip_validates_typed_ids() {
    let parsed = ProvidersDocument::from_yaml(PROVIDERS_YAML).unwrap();
    assert_eq!(parsed.providers[0].id.as_str(), "compatible-primary");

    let legacy = PROVIDERS_YAML.replace(
        "  manage_user_config: true",
        "  manage_user_config: true\n  catalog_path: /tmp/legacy-codex-models.json",
    );
    ProvidersDocument::from_yaml(&legacy).expect("legacy catalog_path remains readable");

    let invalid = PROVIDERS_YAML.replace("compatible-primary", "INVALID/provider");
    let error = ProvidersDocument::from_yaml(&invalid).unwrap_err();
    assert!(error.to_string().contains("invalid provider id"));
}

#[test]
fn chat_completions_provider_is_protocol_typed_and_http_only() {
    let yaml = PROVIDERS_YAML
        .replace("openai_responses", "openai_chat_completions")
        .replace("Compatible Primary", "Chat Completions Primary");
    let parsed = ProvidersDocument::from_yaml(&yaml).unwrap();
    assert_eq!(
        parsed.providers[0].protocol,
        ProtocolId::OpenaiChatCompletions
    );

    let invalid = yaml
        .replace("websocket: null", "websocket: wss://gateway.example.com/v1")
        .replace("websocket: false", "websocket: true");
    let error = ProvidersDocument::from_yaml(&invalid).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("has no native WebSocket transport")
    );
}

#[test]
fn anthropic_provider_is_protocol_typed_and_http_only() {
    let yaml = PROVIDERS_YAML
        .replace("openai_responses", "anthropic_messages")
        .replace("Compatible Primary", "Anthropic Primary");
    let parsed = ProvidersDocument::from_yaml(&yaml).unwrap();
    assert_eq!(parsed.providers[0].protocol, ProtocolId::AnthropicMessages);

    let invalid = yaml
        .replace("websocket: null", "websocket: wss://gateway.example.com/v1")
        .replace("websocket: false", "websocket: true");
    let error = ProvidersDocument::from_yaml(&invalid).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("has no native WebSocket transport")
    );
}

#[test]
fn cache_yaml_rejects_invalid_upstream_model_ids() {
    let yaml = r#"
schema_version: 1
providers:
  compatible-primary:
    config_fingerprint: sha256:abc
    last_successful_refresh_at: 2026-08-11T10:00:00Z
    source:
      protocol: openai_responses
      endpoint: https://gateway.example.com/v1/models
    models:
      - upstream_model_id: " coder "
        catalog_model_id: compatible-primary/coder
        display_name: Coder
        publication_status: ready
        context_window: 128000
        supported_reasoning_levels: []
        supports_parallel_tool_calls: true
        supports_search_tool: false
        metadata_sources: {}
"#;
    assert!(ModelCacheDocument::from_yaml(yaml).is_err());
}

#[test]
fn cache_validation_rejects_catalog_id_mismatch() {
    let configured = provider("provider-a", false);
    let mut provider_cache =
        cached_provider(&configured, &[("coder", ModelPublicationStatus::Ready)]);
    provider_cache.models[0].catalog_model_id = CatalogModelId::parse("provider-b/coder").unwrap();
    let cache = ModelCacheDocument {
        schema_version: 1,
        providers: BTreeMap::from([(configured.id, provider_cache)]),
    };
    assert!(cache.validate().is_err());
}

#[test]
fn enabled_models_can_use_conservative_catalog_defaults() {
    let configured = provider("provider-a", false);
    let mut provider_cache =
        cached_provider(&configured, &[("coder", ModelPublicationStatus::Ready)]);
    provider_cache.models[0].context_window = None;
    let cache = ModelCacheDocument {
        schema_version: 1,
        providers: BTreeMap::from([(configured.id, provider_cache)]),
    };
    assert!(cache.validate().is_ok());
}
