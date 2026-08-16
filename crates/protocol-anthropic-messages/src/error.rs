use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AnthropicProtocolError {
    #[error(transparent)]
    Chat(#[from] protocol_openai_chat_completions::ChatProtocolError),

    #[error("converted Anthropic Messages request is malformed")]
    InvalidRequest,

    #[error("Responses request uses an item or option unsupported by Anthropic Messages: {0}")]
    UnsupportedInput(String),

    #[error("converted Anthropic Messages request could not be serialized: {0}")]
    Serialization(String),

    #[error("Anthropic Messages bridge session history exceeds its configured limit")]
    SessionHistoryLimit,

    #[error("upstream Anthropic Messages stream is malformed")]
    InvalidStream,

    #[error("upstream Anthropic Messages event exceeds its configured buffer limit")]
    StreamBufferLimit,

    #[error("upstream Anthropic Messages stream aggregation exceeds its configured limit")]
    StreamStateLimit,
}
