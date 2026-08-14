use std::collections::BTreeMap;

use provider_x_core::DiscoveredModel;
use serde_json::Value;

use crate::ChatProtocolError;

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
    let root: Value = serde_json::from_slice(bytes)
        .map_err(|error| ChatProtocolError::InvalidJson(error.to_string()))?;
    let items = match &root {
        Value::Array(items) => items,
        Value::Object(object) => object
            .get("data")
            .or_else(|| object.get("models"))
            .and_then(Value::as_array)
            .ok_or(ChatProtocolError::InvalidStream)?,
        _ => return Err(ChatProtocolError::InvalidStream),
    };
    let mut models = BTreeMap::new();
    for item in items {
        let id = item
            .as_str()
            .or_else(|| item.get("id").and_then(Value::as_str))
            .map(str::trim)
            .filter(|id| !id.is_empty());
        if let Some(id) = id {
            models.entry(id.to_owned()).or_insert(DiscoveredModel {
                id: id.to_owned(),
                display_name: item
                    .get("display_name")
                    .or_else(|| item.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                context_window: item
                    .get("context_window")
                    .or_else(|| item.get("context_length"))
                    .and_then(Value::as_u64),
                supported_reasoning_levels: None,
                supports_parallel_tool_calls: item
                    .get("supports_parallel_tool_calls")
                    .and_then(Value::as_bool),
                supports_search_tool: item.get("supports_search_tool").and_then(Value::as_bool),
            });
        }
    }
    if models.is_empty() {
        return Err(ChatProtocolError::InvalidStream);
    }
    Ok(models.into_values().collect())
}
