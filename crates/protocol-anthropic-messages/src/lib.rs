mod bridge;
mod error;
mod model_list;

pub use bridge::{
    AnthropicBridgeRequest, AnthropicMessagesWsHttpAdapter, AnthropicSseDecoder,
    AnthropicStreamOutcome, prepare_http_request, prepare_http_request_with_thinking_mode,
};
pub use error::AnthropicProtocolError;
pub use model_list::{model_list_url, parse_model_list};

#[must_use]
pub fn messages_url(http_endpoint: &str) -> String {
    format!("{}/v1/messages", http_endpoint.trim_end_matches('/'))
}
