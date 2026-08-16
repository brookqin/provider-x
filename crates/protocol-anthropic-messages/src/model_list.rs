use provider_x_core::DiscoveredModel;

use crate::AnthropicProtocolError;

#[must_use]
pub fn model_list_url(http_endpoint: &str) -> String {
    format!("{}/v1/models", http_endpoint.trim_end_matches('/'))
}

/// Parses the Anthropic-compatible model list.
///
/// # Errors
///
/// Returns an error for malformed JSON or a list without usable model IDs.
pub fn parse_model_list(bytes: &[u8]) -> Result<Vec<DiscoveredModel>, AnthropicProtocolError> {
    protocol_openai_chat_completions::parse_model_list(bytes).map_err(Into::into)
}
