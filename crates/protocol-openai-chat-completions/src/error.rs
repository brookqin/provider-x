use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ChatProtocolError {
    #[error("request body is not valid Responses JSON: {0}")]
    InvalidJson(String),

    #[error("request body must be a JSON object")]
    BodyMustBeObject,

    #[error("WebSocket message must be response.create")]
    UnsupportedWebSocketMessage,

    #[error("Responses request uses an item or tool unsupported by Chat Completions: {0}")]
    UnsupportedInput(String),

    #[error("converted Chat Completions request could not be serialized: {0}")]
    Serialization(String),

    #[error("Chat Completions bridge session history exceeds its configured limit")]
    SessionHistoryLimit,

    #[error("upstream Chat Completions stream is malformed")]
    InvalidStream,

    #[error("upstream Chat Completions event exceeds its configured buffer limit")]
    StreamBufferLimit,

    #[error("upstream Chat Completions stream aggregation exceeds its configured limit")]
    StreamStateLimit,
}
