mod bridge;
mod error;
mod model_list;

pub use bridge::{
    ChatBridgeAction, ChatBridgeRequest, ChatCompletionsWsHttpAdapter, ChatSseDecoder,
    ChatStreamOutcome, ResponsesChatBridgeSession, prepare_http_request,
};
pub use error::ChatProtocolError;
pub use model_list::{model_list_url, parse_model_list};

#[must_use]
pub fn chat_completions_url(http_endpoint: &str) -> String {
    format!("{}/chat/completions", http_endpoint.trim_end_matches('/'))
}
