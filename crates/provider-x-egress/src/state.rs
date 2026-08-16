use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use arc_swap::ArcSwap;
use bytes::Bytes;
use http_body_util::Full;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use provider_x_catalog::CatalogOverlay;
use provider_x_core::{
    ModelCacheDocument, ProviderConfig, ProvidersDocument, ProxyEnvironment, RouteResolver,
    RuntimeSnapshot,
};
use provider_x_network::{NetworkConnector, build_http_connector, build_websocket_connector};
use provider_x_providers::{ProviderProfile, build_runtime_snapshot, resolve_provider};
use tokio::sync::Semaphore;

use crate::{EgressBuildError, EgressEvent, EgressObserver, events::NoopObserver};

pub(crate) type UpstreamClient = Client<NetworkConnector, Full<Bytes>>;

#[derive(Clone)]
pub(crate) struct ProviderEgress {
    pub(crate) config: ProviderConfig,
    pub(crate) profile: ProviderProfile,
    pub(crate) client: UpstreamClient,
    pub(crate) websocket_connector: NetworkConnector,
}

pub(crate) struct EgressRuntimeSnapshot {
    routes: RuntimeSnapshot,
    providers: BTreeMap<provider_x_core::ProviderId, ProviderEgress>,
    pub(crate) catalog_overlay: CatalogOverlay,
}

pub struct PreparedEgressReload {
    runtime: Arc<EgressRuntimeSnapshot>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct IngressCapability(Arc<str>);

impl IngressCapability {
    /// Parses a 256-bit capability encoded as lowercase hexadecimal.
    ///
    /// # Errors
    ///
    /// Returns an error unless the value is exactly 64 lowercase hexadecimal characters.
    pub fn from_hex(value: impl Into<Arc<str>>) -> Result<Self, EgressBuildError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(EgressBuildError::InvalidIngressCapability);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    fn matches(&self, candidate: &str) -> bool {
        if candidate.len() != self.0.len() {
            return false;
        }
        self.0
            .bytes()
            .zip(candidate.bytes())
            .fold(0_u8, |difference, (expected, actual)| {
                difference | (expected ^ actual)
            })
            == 0
    }
}

impl fmt::Debug for IngressCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("IngressCapability([REDACTED])")
    }
}

impl EgressRuntimeSnapshot {
    pub(crate) fn resolve(&self, model: &str) -> provider_x_core::RouteDecision {
        self.routes.resolve(model)
    }

    pub(crate) fn provider(
        &self,
        provider_id: &provider_x_core::ProviderId,
    ) -> Option<&ProviderEgress> {
        self.providers.get(provider_id)
    }
}

#[derive(Clone)]
pub struct EgressState {
    pub(crate) runtime: Arc<ArcSwap<EgressRuntimeSnapshot>>,
    pub(crate) official_base_url: Arc<str>,
    pub(crate) request_body_limit_bytes: usize,
    pub(crate) request_body_timeout_ms: u64,
    pub(crate) response_headers_timeout_ms: u64,
    pub(crate) stream_idle_timeout_ms: u64,
    pub(crate) websocket_idle_timeout_ms: u64,
    pub(crate) shutdown_grace_ms: u64,
    pub(crate) connection_limit: Arc<Semaphore>,
    pub(crate) official_client: UpstreamClient,
    pub(crate) official_websocket_connector: NetworkConnector,
    pub(crate) websocket_fallback_on_upgrade: bool,
    websocket_session_sequence: Arc<AtomicU64>,
    ingress_capability: IngressCapability,
    observer: Arc<dyn EgressObserver>,
}

impl EgressState {
    /// Creates immutable HTTP routing state and a pooled HTTP/TLS client.
    ///
    /// # Errors
    ///
    /// Returns an error when the Provider document is invalid or contains duplicate IDs.
    pub fn new(
        providers: &ProvidersDocument,
        cache: &ModelCacheDocument,
        official_base_url: impl Into<Arc<str>>,
        ingress_capability: IngressCapability,
    ) -> Result<Self, EgressBuildError> {
        providers.validate()?;
        let proxy_environment = ProxyEnvironment::read();
        let provider_map = build_provider_map(providers, &proxy_environment)?;
        let catalog_overlay = CatalogOverlay::from_documents(providers, cache)?;

        let (official_client, official_websocket_connector) =
            build_transport(providers, &proxy_environment)?;
        Ok(Self {
            runtime: Arc::new(ArcSwap::from_pointee(EgressRuntimeSnapshot {
                routes: build_runtime_snapshot(providers, cache)?,
                providers: provider_map,
                catalog_overlay,
            })),
            official_base_url: official_base_url.into(),
            request_body_limit_bytes: usize::try_from(providers.listener.request_body_limit_bytes)
                .unwrap_or(usize::MAX),
            request_body_timeout_ms: providers.timeouts.request_body_ms,
            response_headers_timeout_ms: providers.timeouts.response_headers_ms,
            stream_idle_timeout_ms: providers.timeouts.stream_idle_ms,
            websocket_idle_timeout_ms: providers.timeouts.websocket_idle_ms,
            shutdown_grace_ms: providers.timeouts.shutdown_grace_ms,
            connection_limit: Arc::new(Semaphore::new(
                usize::try_from(providers.listener.max_connections).unwrap_or(usize::MAX),
            )),
            official_client,
            official_websocket_connector,
            websocket_fallback_on_upgrade: false,
            websocket_session_sequence: Arc::new(AtomicU64::new(1)),
            ingress_capability,
            observer: Arc::new(NoopObserver),
        })
    }

    /// Atomically replaces the routing table, Provider credentials/endpoints, and Provider HTTP
    /// clients for new requests. Existing requests keep their previously loaded snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid Provider/cache state or proxy client construction failure.
    pub fn reload(
        &self,
        providers: &ProvidersDocument,
        cache: &ModelCacheDocument,
    ) -> Result<(), EgressBuildError> {
        let prepared = self.prepare_reload(providers, cache)?;
        self.commit_reload(prepared);
        Ok(())
    }

    /// Builds a complete replacement without exposing it to requests yet.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid Provider/cache state or proxy client construction failure.
    pub fn prepare_reload(
        &self,
        providers: &ProvidersDocument,
        cache: &ModelCacheDocument,
    ) -> Result<PreparedEgressReload, EgressBuildError> {
        providers.validate()?;
        let routes = build_runtime_snapshot(providers, cache)?;
        let proxy_environment = ProxyEnvironment::read();
        let provider_map = build_provider_map(providers, &proxy_environment)?;
        let catalog_overlay = CatalogOverlay::from_documents(providers, cache)?;
        Ok(PreparedEgressReload {
            runtime: Arc::new(EgressRuntimeSnapshot {
                routes,
                providers: provider_map,
                catalog_overlay,
            }),
        })
    }

    /// Publishes a fully prepared replacement in one atomic pointer swap.
    pub fn commit_reload(&self, prepared: PreparedEgressReload) {
        self.runtime.store(prepared.runtime);
    }

    /// Installs a redacted event observer used by contract probes and diagnostics.
    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn EgressObserver>) -> Self {
        self.observer = observer;
        self
    }

    /// Makes WebSocket upgrade attempts return HTTP 426 so a real Codex client can exercise its
    /// documented HTTP fallback path. Production state leaves this disabled.
    #[must_use]
    pub fn with_websocket_fallback_on_upgrade(mut self) -> Self {
        self.websocket_fallback_on_upgrade = true;
        self
    }

    pub(crate) fn observe(&self, event: EgressEvent) {
        self.observer.record(event);
    }

    pub(crate) fn next_websocket_session_id(&self) -> u64 {
        self.websocket_session_sequence
            .fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn authorized_path<'a>(&self, path: &'a str) -> Option<&'a str> {
        let separator = path.strip_prefix('/')?.find('/')? + 1;
        let candidate = &path[1..separator];
        self.ingress_capability
            .matches(candidate)
            .then_some(&path[separator..])
    }
}

fn build_provider_map(
    providers: &ProvidersDocument,
    proxy_environment: &ProxyEnvironment,
) -> Result<BTreeMap<provider_x_core::ProviderId, ProviderEgress>, EgressBuildError> {
    let mut provider_map = BTreeMap::new();
    for provider in &providers.providers {
        let (client, websocket_connector) = build_transport(providers, proxy_environment)?;
        if provider_map
            .insert(
                provider.id.clone(),
                ProviderEgress {
                    profile: resolve_provider(provider),
                    config: provider.clone(),
                    client,
                    websocket_connector,
                },
            )
            .is_some()
        {
            return Err(EgressBuildError::DuplicateProvider(provider.id.to_string()));
        }
    }
    Ok(provider_map)
}

fn build_transport(
    providers: &ProvidersDocument,
    proxy_environment: &ProxyEnvironment,
) -> Result<(UpstreamClient, NetworkConnector), EgressBuildError> {
    let connect_timeout = std::time::Duration::from_millis(providers.timeouts.connect_ms);
    let http_connector = build_http_connector(proxy_environment, connect_timeout)?;
    let websocket_connector = build_websocket_connector(proxy_environment, connect_timeout)?;
    Ok((
        Client::builder(TokioExecutor::new()).build(http_connector),
        websocket_connector,
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use provider_x_core::{
        AuthConfig, CatalogModelId, CodexConfig, EndpointConfig, ListenerConfig,
        ModelCacheDocument, ModelId, ModelPublicationStatus, ProtocolId, ProviderConfig,
        ProviderId, ProviderModelCache, ProviderModelSource, ProviderModelSpec, ProvidersDocument,
        TimeoutConfig, TransportConfig,
    };

    use super::{EgressState, IngressCapability};

    fn documents(endpoint: &str) -> (ProvidersDocument, ModelCacheDocument) {
        let provider_id = ProviderId::new("provider-a").unwrap();
        let model_id = ModelId::new("coder").unwrap();
        let provider = ProviderConfig {
            id: provider_id.clone(),
            name: "Provider A".to_owned(),
            description: None,
            enabled: true,
            kind: provider_x_core::ProviderKind::Custom,
            protocol: ProtocolId::OpenaiResponses,
            anthropic_thinking: None,
            endpoints: EndpointConfig {
                http: endpoint.to_owned(),
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
        let fingerprint = provider_x_providers::resolve_provider(&provider)
            .routing_fingerprint()
            .unwrap();
        let providers = ProvidersDocument {
            schema_version: provider_x_core::SCHEMA_VERSION,
            listener: ListenerConfig {
                host: "127.0.0.1".to_owned(),
                port: 43_119,
                request_body_limit_bytes: 1024,
                max_connections: 4,
            },
            timeouts: TimeoutConfig {
                request_body_ms: 1000,
                connect_ms: 1000,
                response_headers_ms: 1000,
                stream_idle_ms: 1000,
                websocket_idle_ms: 1000,
                shutdown_grace_ms: 1000,
            },
            codex: CodexConfig {
                manage_user_config: false,
            },
            providers: vec![provider],
        };
        let cache = ModelCacheDocument {
            schema_version: 1,
            providers: BTreeMap::from([(
                provider_id.clone(),
                ProviderModelCache {
                    config_fingerprint: fingerprint,
                    last_successful_refresh_at: "now".to_owned(),
                    source: ProviderModelSource {
                        protocol: ProtocolId::OpenaiResponses,
                        endpoint: format!("{endpoint}/models"),
                    },
                    models: vec![ProviderModelSpec {
                        upstream_model_id: model_id.clone(),
                        catalog_model_id: CatalogModelId::for_provider(&provider_id, &model_id),
                        display_name: "Coder".to_owned(),
                        publication_status: ModelPublicationStatus::Ready,
                        context_window: Some(128_000),
                        supported_reasoning_levels: Vec::new(),
                        supports_parallel_tool_calls: Some(false),
                        supports_search_tool: Some(false),
                        metadata_sources: BTreeMap::new(),
                    }],
                },
            )]),
        };
        (providers, cache)
    }

    #[test]
    fn reload_atomically_replaces_routes_and_provider_runtime_for_new_requests() {
        let (first_providers, first_cache) = documents("https://first.example/v1");
        let state = EgressState::new(
            &first_providers,
            &first_cache,
            "https://chatgpt.com/backend-api/codex",
            IngressCapability::from_hex("0".repeat(64)).unwrap(),
        )
        .unwrap();
        let old = state.runtime.load_full();
        let provider_id = ProviderId::new("provider-a").unwrap();
        assert_eq!(
            old.provider(&provider_id).unwrap().config.endpoints.http,
            "https://first.example/v1"
        );

        let (second_providers, second_cache) = documents("https://second.example/v1");
        state.reload(&second_providers, &second_cache).unwrap();
        let current = state.runtime.load_full();

        assert_eq!(
            current
                .provider(&provider_id)
                .unwrap()
                .config
                .endpoints
                .http,
            "https://second.example/v1"
        );
        assert_eq!(
            old.provider(&provider_id).unwrap().config.endpoints.http,
            "https://first.example/v1"
        );
    }

    #[test]
    fn ingress_capability_requires_canonical_256_bit_hex_and_redacts_debug() {
        let value = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let capability = IngressCapability::from_hex(value).unwrap();
        assert_eq!(capability.expose(), value);
        assert!(!format!("{capability:?}").contains(value));
        assert!(IngressCapability::from_hex("0".repeat(63)).is_err());
        assert!(IngressCapability::from_hex("A".repeat(64)).is_err());
        assert!(IngressCapability::from_hex("g".repeat(64)).is_err());
    }
}
