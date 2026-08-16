use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Limited};
use hyper::{
    Method, Request, StatusCode,
    header::{ACCEPT, ACCEPT_ENCODING, ETAG, HeaderValue, IF_NONE_MATCH, USER_AGENT},
};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use provider_x_core::{DiscoveredModel, ProviderConfig, ProviderModelCache, ProxyEnvironment};
use provider_x_network::{NetworkConnector, build_http_connector};
use provider_x_providers::{resolve_provider, validate_provider};

use crate::{
    CatalogError, MODEL_REGISTRY_SCHEMA_VERSION, MODEL_REGISTRY_URL, ModelRegistryCache,
    RefreshPreview, RegistryEnrichment, build_refresh_preview,
    model_registry::apply_registry_suggestions,
};

type DiscoveryHttpClient = Client<NetworkConnector, Empty<Bytes>>;

#[derive(Clone, Debug)]
pub struct ManualDiscoveryClient {
    client: DiscoveryHttpClient,
    response_timeout: Duration,
    response_body_limit_bytes: usize,
    model_registry_url: Arc<str>,
}

impl ManualDiscoveryClient {
    /// Builds the control-plane client. It performs no request until `discover` is called.
    ///
    /// # Errors
    ///
    /// Returns an error if the TLS/proxy connector cannot be initialized or the configured proxy
    /// URL is invalid.
    pub fn new(
        connect_timeout: Duration,
        response_timeout: Duration,
        response_body_limit_bytes: usize,
    ) -> Result<Self, CatalogError> {
        let proxy_environment = ProxyEnvironment::read();
        let connector = build_http_connector(&proxy_environment, connect_timeout)?;
        let client = Client::builder(TokioExecutor::new()).build(connector);
        if response_body_limit_bytes == 0 {
            return Err(CatalogError::DiscoveryClient);
        }
        Ok(Self {
            client,
            response_timeout,
            response_body_limit_bytes,
            model_registry_url: Arc::from(MODEL_REGISTRY_URL),
        })
    }

    #[cfg(test)]
    fn with_model_registry_url(mut self, url: impl Into<Arc<str>>) -> Self {
        self.model_registry_url = url.into();
        self
    }

    /// Executes the protocol-fixed model-list request for one explicit user refresh action.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid Provider configuration, non-success status, timeout, oversized
    /// body, transport failure, or malformed protocol response.
    pub async fn discover(
        &self,
        provider: &ProviderConfig,
    ) -> Result<Vec<DiscoveredModel>, CatalogError> {
        validate_provider(provider)?;
        let profile = resolve_provider(provider);
        self.discover_provider_models(provider, &profile).await
    }

    /// Discovers models and builds a cache preview without mutating the previous cache.
    ///
    /// # Errors
    ///
    /// Returns the same errors as `discover` plus cache construction errors.
    pub async fn refresh_preview(
        &self,
        provider: &ProviderConfig,
        existing: Option<&ProviderModelCache>,
        refreshed_at: impl Into<String>,
    ) -> Result<RefreshPreview, CatalogError> {
        let discovered = self.discover(provider).await?;
        build_refresh_preview(provider, discovered, existing, refreshed_at)
    }

    /// Fetches the fixed models.dev registry during an explicit refresh and applies only exact,
    /// missing-field suggestions to the existing preview.
    ///
    /// Registry failures are returned as a warning and never discard Provider discovery results.
    /// A valid previous registry cache remains usable when the network request fails.
    pub async fn enrich_preview_with_registry(
        &self,
        provider: &ProviderConfig,
        preview: &mut RefreshPreview,
        cached: Option<&ModelRegistryCache>,
        fetched_at: impl Into<String>,
    ) -> RegistryEnrichment {
        let cached = cached.filter(|cache| cache.validate().is_ok());
        let fetched = self.fetch_model_registry(cached, fetched_at.into()).await;
        let (effective, replacement_cache, mut warning) = match fetched {
            Ok(RegistryFetch::Updated(cache)) => (Some(cache.clone()), Some(cache), None),
            Ok(RegistryFetch::NotModified) if cached.is_some() => (cached.cloned(), None, None),
            Ok(RegistryFetch::NotModified) => (
                None,
                None,
                Some("model registry returned not-modified without a local cache".to_owned()),
            ),
            Err(error) => (cached.cloned(), None, Some(error.to_string())),
        };
        let matched_models = effective.as_ref().map_or_else(Vec::new, |cache| {
            apply_registry_suggestions(provider, preview, cache).unwrap_or_else(|error| {
                warning = Some(match warning.take() {
                    Some(previous) => format!("{previous}; {error}"),
                    None => error.to_string(),
                });
                Vec::new()
            })
        });
        RegistryEnrichment {
            replacement_cache,
            matched_models,
            warning,
        }
    }

    async fn discover_provider_models(
        &self,
        provider: &ProviderConfig,
        profile: &provider_x_providers::ProviderProfile,
    ) -> Result<Vec<DiscoveredModel>, CatalogError> {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri(profile.model_list_url())
            .header(ACCEPT, "application/json")
            .header(ACCEPT_ENCODING, "identity")
            .header(USER_AGENT, "provider-x/0.1")
            .body(Empty::new())
            .map_err(|_| CatalogError::DiscoveryRequest)?;
        profile.apply_authentication(&provider.auth, request.headers_mut())?;
        let response = tokio::time::timeout(self.response_timeout, self.client.request(request))
            .await
            .map_err(|_| CatalogError::DiscoveryTimeout)?
            .map_err(|_| CatalogError::DiscoveryTransport)?;
        if response.status() != StatusCode::OK {
            return Err(CatalogError::DiscoveryStatus(response.status().as_u16()));
        }
        let limited = Limited::new(response.into_body(), self.response_body_limit_bytes);
        let body = tokio::time::timeout(self.response_timeout, limited.collect())
            .await
            .map_err(|_| CatalogError::DiscoveryTimeout)?
            .map_err(|_| CatalogError::DiscoveryBodyTooLarge(self.response_body_limit_bytes))?
            .to_bytes();
        profile.parse_model_list(&body).map_err(Into::into)
    }

    async fn fetch_model_registry(
        &self,
        cached: Option<&ModelRegistryCache>,
        fetched_at: String,
    ) -> Result<RegistryFetch, CatalogError> {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri(self.model_registry_url.as_ref())
            .header(ACCEPT, "application/json")
            .header(ACCEPT_ENCODING, "identity")
            .header(USER_AGENT, "provider-x/0.1");
        if let Some(etag) = cached.and_then(|cache| cache.etag.as_deref()) {
            let etag = HeaderValue::from_str(etag).map_err(|_| {
                CatalogError::InvalidModelRegistry("invalid cached ETag".to_owned())
            })?;
            request = request.header(IF_NONE_MATCH, etag);
        }
        let request = request
            .body(Empty::new())
            .map_err(|_| CatalogError::DiscoveryRequest)?;
        let response = tokio::time::timeout(self.response_timeout, self.client.request(request))
            .await
            .map_err(|_| CatalogError::DiscoveryTimeout)?
            .map_err(|_| CatalogError::DiscoveryTransport)?;
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(RegistryFetch::NotModified);
        }
        if response.status() != StatusCode::OK {
            return Err(CatalogError::ModelRegistryStatus(
                response.status().as_u16(),
            ));
        }
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let limited = Limited::new(response.into_body(), self.response_body_limit_bytes);
        let body = tokio::time::timeout(self.response_timeout, limited.collect())
            .await
            .map_err(|_| CatalogError::DiscoveryTimeout)?
            .map_err(|_| CatalogError::DiscoveryBodyTooLarge(self.response_body_limit_bytes))?
            .to_bytes();
        let payload = serde_json::from_slice(&body)
            .map_err(|error| CatalogError::InvalidModelRegistry(error.to_string()))?;
        let cache = ModelRegistryCache {
            schema_version: MODEL_REGISTRY_SCHEMA_VERSION,
            source_url: MODEL_REGISTRY_URL.to_owned(),
            fetched_at,
            etag,
            payload,
        };
        cache.validate()?;
        Ok(RegistryFetch::Updated(cache))
    }
}

enum RegistryFetch {
    NotModified,
    Updated(ModelRegistryCache),
}

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, time::Duration};

    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::{Request, Response, StatusCode, body::Incoming, service::service_fn};
    use hyper_util::rt::TokioIo;
    use provider_x_core::{
        AuthConfig, DiscoveredModel, EndpointConfig, ProtocolId, ProviderConfig, ProviderId,
        TransportConfig,
    };
    use serde_json::json;

    use super::ManualDiscoveryClient;
    use crate::{
        MODEL_REGISTRY_SCHEMA_VERSION, MODEL_REGISTRY_URL, ModelRegistryCache,
        build_refresh_preview,
    };

    fn provider(endpoint: String) -> ProviderConfig {
        ProviderConfig {
            id: ProviderId::new("provider-a").unwrap(),
            name: "Provider A".to_owned(),
            description: None,
            enabled: false,
            kind: provider_x_core::ProviderKind::Custom,
            protocol: ProtocolId::OpenaiResponses,
            anthropic_thinking: None,
            endpoints: EndpointConfig {
                http: endpoint,
                websocket: None,
                models: None,
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

    fn incomplete_preview(provider: &ProviderConfig) -> crate::RefreshPreview {
        build_refresh_preview(
            provider,
            vec![DiscoveredModel {
                id: "coder".to_owned(),
                display_name: None,
                context_window: None,
                supported_reasoning_levels: None,
                supports_parallel_tool_calls: None,
                supports_search_tool: None,
            }],
            None,
            "now",
        )
        .unwrap()
    }

    fn registry_cache() -> ModelRegistryCache {
        ModelRegistryCache {
            schema_version: MODEL_REGISTRY_SCHEMA_VERSION,
            source_url: MODEL_REGISTRY_URL.to_owned(),
            fetched_at: "now".to_owned(),
            etag: Some("registry-v1".to_owned()),
            payload: json!({
                "provider-a": {
                    "id": "provider-a",
                    "models": {
                        "coder": {
                            "id": "coder",
                            "name": "Coder Suggested",
                            "limit": {"context": 128_000}
                        }
                    }
                }
            }),
        }
    }

    #[tokio::test]
    async fn explicit_discovery_uses_fixed_path_and_provider_bearer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder.keep_alive(false);
            builder
                .serve_connection(
                    TokioIo::new(stream),
                    service_fn(|request: Request<Incoming>| async move {
                        assert_eq!(request.uri().path(), "/v1/models");
                        assert_eq!(
                            request.headers()[hyper::header::AUTHORIZATION],
                            "Bearer provider-secret"
                        );
                        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                            br#"{"data":[{"id":"coder","context_window":128000,"supports_parallel_tool_calls":true,"supports_search_tool":false}]}"#,
                        ))))
                    }),
                )
                .await
                .unwrap();
        });
        let client =
            ManualDiscoveryClient::new(Duration::from_secs(1), Duration::from_secs(1), 64 * 1024)
                .unwrap();
        let models = client
            .discover(&provider(format!("http://{address}/v1")))
            .await
            .unwrap();
        assert_eq!(models[0].id, "coder");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn anthropic_discovery_uses_typed_override_and_api_key_header() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            hyper::server::conn::http1::Builder::new()
                .keep_alive(false)
                .serve_connection(
                    TokioIo::new(stream),
                    service_fn(|request: Request<Incoming>| async move {
                        assert_eq!(request.uri().path(), "/provider-models");
                        assert!(!request.headers().contains_key(hyper::header::AUTHORIZATION));
                        assert_eq!(request.headers()["x-api-key"], "provider-secret");
                        assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
                        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                            br#"{"data":[{"id":"coder"}]}"#,
                        ))))
                    }),
                )
                .await
                .unwrap();
        });
        let mut configured = provider("https://messages.invalid/anthropic".to_owned());
        configured.protocol = ProtocolId::AnthropicMessages;
        configured.endpoints.models = Some(format!("http://{address}/provider-models"));
        let client =
            ManualDiscoveryClient::new(Duration::from_secs(1), Duration::from_secs(1), 64 * 1024)
                .unwrap();
        let models = client.discover(&configured).await.unwrap();
        assert_eq!(models[0].id, "coder");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn non_success_does_not_expose_provider_secret() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder.keep_alive(false);
            builder
                .serve_connection(
                    TokioIo::new(stream),
                    service_fn(|_: Request<Incoming>| async move {
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(StatusCode::UNAUTHORIZED)
                                .body(Full::new(Bytes::new()))
                                .unwrap(),
                        )
                    }),
                )
                .await
                .unwrap();
        });
        let client =
            ManualDiscoveryClient::new(Duration::from_secs(1), Duration::from_secs(1), 64 * 1024)
                .unwrap();
        let error = client
            .discover(&provider(format!("http://{address}/v1")))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "model discovery returned HTTP 401");
        assert!(!error.to_string().contains("provider-secret"));
    }

    #[tokio::test]
    async fn explicit_registry_fetch_uses_fixed_path_and_exact_identity() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            hyper::server::conn::http1::Builder::new()
                .keep_alive(false)
                .serve_connection(
                    TokioIo::new(stream),
                    service_fn(|request: Request<Incoming>| async move {
                        assert_eq!(request.uri().path(), "/api.json");
                        assert!(!request.headers().contains_key(hyper::header::AUTHORIZATION));
                        Ok::<_, Infallible>(
                            Response::builder()
                                .header(hyper::header::ETAG, "registry-v1")
                                .body(Full::new(Bytes::from(
                                    serde_json::to_vec(&registry_cache().payload).unwrap(),
                                )))
                                .unwrap(),
                        )
                    }),
                )
                .await
                .unwrap();
        });
        let provider = provider("https://provider.invalid/v1".to_owned());
        let mut preview = incomplete_preview(&provider);
        let client =
            ManualDiscoveryClient::new(Duration::from_secs(1), Duration::from_secs(1), 64 * 1024)
                .unwrap()
                .with_model_registry_url(format!("http://{address}/api.json"));

        let enrichment = client
            .enrich_preview_with_registry(&provider, &mut preview, None, "later")
            .await;

        assert_eq!(enrichment.matched_models.len(), 1);
        assert!(enrichment.replacement_cache.is_some());
        assert_eq!(enrichment.warning, None);
        assert_eq!(preview.cache.models[0].context_window, Some(128_000));
        assert_eq!(preview.cache.models[0].supports_parallel_tool_calls, None);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn registry_failure_keeps_provider_preview_and_uses_valid_cache() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            hyper::server::conn::http1::Builder::new()
                .keep_alive(false)
                .serve_connection(
                    TokioIo::new(stream),
                    service_fn(|request: Request<Incoming>| async move {
                        assert_eq!(
                            request.headers()[hyper::header::IF_NONE_MATCH],
                            "registry-v1"
                        );
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(StatusCode::SERVICE_UNAVAILABLE)
                                .body(Full::new(Bytes::new()))
                                .unwrap(),
                        )
                    }),
                )
                .await
                .unwrap();
        });
        let provider = provider("https://provider.invalid/v1".to_owned());
        let mut preview = incomplete_preview(&provider);
        let cache = registry_cache();
        let client =
            ManualDiscoveryClient::new(Duration::from_secs(1), Duration::from_secs(1), 64 * 1024)
                .unwrap()
                .with_model_registry_url(format!("http://{address}/api.json"));

        let enrichment = client
            .enrich_preview_with_registry(&provider, &mut preview, Some(&cache), "later")
            .await;

        assert_eq!(enrichment.matched_models.len(), 1);
        assert!(enrichment.replacement_cache.is_none());
        assert!(enrichment.warning.unwrap().contains("HTTP 503"));
        assert_eq!(preview.cache.models[0].display_name, "Coder Suggested");
        assert_eq!(preview.cache.models[0].context_window, Some(128_000));
        server.await.unwrap();
    }
}
