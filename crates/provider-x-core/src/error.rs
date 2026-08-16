use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoreError {
    #[error("unsupported schema version {actual}; expected {expected}")]
    UnsupportedSchemaVersion { actual: u32, expected: u32 },

    #[error("listener must bind to 127.0.0.1, got {0}")]
    InvalidListenerHost(String),

    #[error("listener port must not be zero")]
    InvalidListenerPort,

    #[error("listener request body limit and max connections must be greater than zero")]
    InvalidListenerLimits,

    #[error("all configured timeouts must be greater than zero")]
    InvalidTimeout,

    #[error(
        "invalid provider id {0:?}; expected non-empty lowercase ASCII letters, digits, or '-'"
    )]
    InvalidProviderId(String),

    #[error("duplicate provider id {0}")]
    DuplicateProviderId(String),

    #[error("invalid model id {0:?}; it must be non-empty and contain no control characters")]
    InvalidModelId(String),

    #[error("provider {provider_id} is enabled but has no model cache")]
    MissingModelCache { provider_id: String },

    #[error("provider {provider_id} cache fingerprint is stale")]
    StaleModelCache { provider_id: String },

    #[error("provider {provider_id} cache contains duplicate model {model_id}")]
    DuplicateModel {
        provider_id: String,
        model_id: String,
    },

    #[error("catalog model id mismatch: expected {expected}, got {actual}")]
    CatalogModelIdMismatch { expected: String, actual: String },

    #[error("provider {provider_id} has an empty bearer API key")]
    EmptyApiKey { provider_id: String },

    #[error("provider {provider_id} has a bearer API key that cannot be used in an HTTP header")]
    InvalidApiKey { provider_id: String },

    #[error("provider {provider_id} HTTP endpoint must be absolute")]
    InvalidHttpEndpoint { provider_id: String },

    #[error("provider {provider_id} model-list endpoint must be an absolute HTTP URL")]
    InvalidModelListEndpoint { provider_id: String },

    #[error("provider {provider_id} declares WebSocket support without a WebSocket endpoint")]
    MissingWebSocketEndpoint { provider_id: String },

    #[error("provider {provider_id} WebSocket endpoint must be absolute ws:// or wss://")]
    InvalidWebSocketEndpoint { provider_id: String },

    #[error("provider {provider_id} uses {protocol}, which has no native WebSocket transport")]
    ProtocolWebSocketUnsupported {
        provider_id: String,
        protocol: &'static str,
    },

    #[error("ready model {model_id} for provider {provider_id} has incomplete capabilities")]
    IncompleteReadyModel {
        provider_id: String,
        model_id: String,
    },

    #[error("failed to serialize provider routing fingerprint: {0}")]
    FingerprintSerialization(String),

    #[error("failed to parse YAML: {0}")]
    InvalidYaml(String),
}
