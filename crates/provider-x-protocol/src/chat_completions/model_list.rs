use provider_x_core::DiscoveredModel;

use crate::chat_completions::ChatProtocolError;

#[must_use]
pub fn model_list_url(http_endpoint: &str) -> String {
    format!("{}/models", http_endpoint.trim_end_matches('/'))
}

/// Parses the OpenAI-compatible model list used by Chat Completions Providers.
///
/// # Errors
///
/// Returns an error for malformed JSON or a list without usable model IDs.
pub fn parse_model_list(bytes: &[u8]) -> Result<Vec<DiscoveredModel>, ChatProtocolError> {
    crate::model_list::parse_model_list(bytes).map_err(|_| ChatProtocolError::InvalidStream)
}
