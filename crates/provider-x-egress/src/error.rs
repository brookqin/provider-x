use thiserror::Error;

#[derive(Debug, Error)]
pub enum EgressBuildError {
    #[error(transparent)]
    InvalidConfiguration(#[from] provider_x_core::CoreError),

    #[error(transparent)]
    InvalidCatalogOverlay(#[from] provider_x_catalog::CatalogError),

    #[error(transparent)]
    ProxyConfiguration(#[from] provider_x_network::ProxyConfigurationError),

    #[error(transparent)]
    Provider(#[from] provider_x_providers::ProviderError),

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

impl ProxyError {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::BodyTooLarge => "body_too_large",
            Self::RequestBodyTimeout => "request_body_timeout",
            Self::UnsupportedContentEncoding => "unsupported_content_encoding",
            Self::InvalidRequest(_) => "invalid_request",
            Self::InvalidWebSocketHandshake => "invalid_websocket_handshake",
            Self::IngressNotFound => "ingress_not_found",
            Self::CrossOriginWebSocket => "cross_origin_websocket",
            Self::ModelNotAvailable => "model_not_available",
            Self::ProviderNotAvailable => "provider_not_available",
            Self::InvalidUpstreamUri => "invalid_upstream_uri",
            Self::RequestBuild => "request_build_failed",
            Self::ResponseHeadersTimeout => "response_headers_timeout",
            Self::ModelCatalogBodyTimeout => "model_catalog_body_timeout",
            Self::ModelCatalogBodyTooLarge => "model_catalog_body_too_large",
            Self::InvalidOfficialModelCatalog => "invalid_official_model_catalog",
            Self::Upstream => "upstream_request_failed",
            Self::UpstreamConnect => "upstream_connect_failed",
        }
    }
}
