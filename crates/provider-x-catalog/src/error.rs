use thiserror::Error;

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error(transparent)]
    Core(#[from] provider_x_core::CoreError),

    #[error(transparent)]
    Provider(#[from] provider_x_providers::ProviderError),

    #[error(transparent)]
    ProxyConfiguration(#[from] provider_x_network::ProxyConfigurationError),

    #[error("invalid Codex model catalog: {0}")]
    InvalidCodexCatalog(String),

    #[error("failed to serialize Codex catalog: {0}")]
    CatalogSerialization(String),

    #[error("failed to build model discovery client")]
    DiscoveryClient,

    #[error("invalid model registry cache: {0}")]
    InvalidModelRegistry(String),

    #[error("model registry returned HTTP {0}")]
    ModelRegistryStatus(u16),

    #[error("failed to build model discovery request")]
    DiscoveryRequest,

    #[error("model discovery request failed")]
    DiscoveryTransport,

    #[error("model discovery timed out")]
    DiscoveryTimeout,

    #[error("model discovery returned HTTP {0}")]
    DiscoveryStatus(u16),

    #[error("model discovery response exceeded {0} bytes")]
    DiscoveryBodyTooLarge(usize),

    #[error("model {0} is not present in the refresh preview")]
    PreviewModelNotFound(String),

    #[error("model display name must not be empty")]
    EmptyModelDisplayName,

    #[error("model context window must be greater than zero")]
    InvalidContextWindow,
}
