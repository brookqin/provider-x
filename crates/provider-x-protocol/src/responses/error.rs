use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("unsupported Responses path {0}")]
    UnsupportedPath(String),

    #[error("request body is not valid JSON: {0}")]
    InvalidJson(String),

    #[error("request body must be a JSON object")]
    BodyMustBeObject,

    #[error("request body must contain a non-empty top-level string model")]
    InvalidModel,

    #[error("WebSocket message must be response.create")]
    UnsupportedWebSocketMessage,

    #[error("rewritten request body could not be serialized: {0}")]
    Serialization(String),

    #[error("Responses bridge session history exceeds its configured limit")]
    SessionHistoryLimit,

    #[error("upstream Responses stream is malformed")]
    InvalidStream,

    #[error("upstream Responses event exceeds its configured buffer limit")]
    StreamBufferLimit,

    #[error("upstream Responses reasoning state exceeds its configured limit")]
    StreamStateLimit,
}
