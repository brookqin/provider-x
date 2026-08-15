use thiserror::Error;

#[derive(Debug, Error)]
pub enum EgressBuildError {
    #[error(transparent)]
    InvalidConfiguration(#[from] provider_x_core::CoreError),

    #[error(transparent)]
    InvalidCatalogOverlay(#[from] provider_x_catalog::CatalogError),

    #[error(transparent)]
    ProxyConfiguration(#[from] provider_x_network::ProxyConfigurationError),

    #[error("duplicate provider {0} in egress state")]
    DuplicateProvider(String),

    #[error("ingress capability must contain exactly 64 lowercase hexadecimal characters")]
    InvalidIngressCapability,
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("request body exceeds configured limit")]
    BodyTooLarge,

    #[error("request body timed out")]
    RequestBodyTimeout,

    #[error("unsupported request content encoding")]
    UnsupportedContentEncoding,

    #[error("invalid Responses request: {0}")]
    InvalidRequest(String),

    #[error("invalid WebSocket upgrade request")]
    InvalidWebSocketHandshake,

    #[error("ingress route not found")]
    IngressNotFound,

    #[error("browser-origin WebSocket connections are forbidden")]
    CrossOriginWebSocket,

    #[error("managed model is not available")]
    ModelNotAvailable,

    #[error("routed Provider is not available")]
    ProviderNotAvailable,

    #[error("invalid upstream URI")]
    InvalidUpstreamUri,

    #[error("failed to build upstream request")]
    RequestBuild,

    #[error("upstream response headers timed out")]
    ResponseHeadersTimeout,

    #[error("official model catalog response timed out")]
    ModelCatalogBodyTimeout,

    #[error("official model catalog response exceeds configured limit")]
    ModelCatalogBodyTooLarge,

    #[error("official model catalog response is invalid")]
    InvalidOfficialModelCatalog,

    #[error("upstream request failed")]
    Upstream,

    #[error("failed to connect to upstream")]
    UpstreamConnect,
}
