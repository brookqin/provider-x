use std::collections::BTreeMap;

pub use provider_x_core::DiscoveredModel;
use serde_json::{Map, Value};

use crate::ProtocolError;

#[must_use]
pub fn model_list_url(http_endpoint: &str) -> String {
    format!("{}/models", http_endpoint.trim_end_matches('/'))
}

/// Parses the common `OpenAI` model-list response shapes without inventing capabilities.
///
/// # Errors
///
/// Returns an error when the response is not JSON, has no supported list shape, or contains no
/// usable model identifiers.
pub fn parse_model_list(bytes: &[u8]) -> Result<Vec<DiscoveredModel>, ProtocolError> {
    let root: Value = serde_json::from_slice(bytes)
        .map_err(|error| ProtocolError::InvalidJson(error.to_string()))?;
    let items = match &root {
        Value::Array(items) => items,
        Value::Object(object) => object
            .get("data")
            .or_else(|| object.get("models"))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                ProtocolError::InvalidJson(
                    "model list must be an array or contain a data/models array".to_owned(),
                )
            })?,
        _ => {
            return Err(ProtocolError::InvalidJson(
                "model list must be a JSON object or array".to_owned(),
            ));
        }
    };

    let mut models = BTreeMap::<String, DiscoveredModel>::new();
    for item in items {
        let Some(model) = parse_model(item) else {
            continue;
        };
        models
            .entry(model.id.clone())
            .and_modify(|existing| merge_missing(existing, &model))
            .or_insert(model);
    }
    if models.is_empty() {
        return Err(ProtocolError::InvalidJson(
            "model list contains no usable model identifiers".to_owned(),
        ));
    }
    Ok(models.into_values().collect())
}

fn parse_model(value: &Value) -> Option<DiscoveredModel> {
    if let Some(id) = value.as_str().and_then(non_empty) {
        return Some(DiscoveredModel {
            id: id.to_owned(),
            display_name: None,
            context_window: None,
            supported_reasoning_levels: None,
            supports_parallel_tool_calls: None,
            supports_search_tool: None,
        });
    }

    let object = value.as_object()?;
    let id = string_field(object, &["id", "slug", "model", "name"])?;
    Some(DiscoveredModel {
        id: id.to_owned(),
        display_name: string_field(object, &["display_name", "displayName", "title"])
            .map(ToOwned::to_owned),
        context_window: unsigned_field(
            object,
            &[
                "context_window",
                "contextWindow",
                "context_length",
                "contextLength",
                "max_context_tokens",
                "maxContextTokens",
            ],
        ),
        supported_reasoning_levels: reasoning_levels(object),
        supports_parallel_tool_calls: bool_field(
            object,
            &["supports_parallel_tool_calls", "supportsParallelToolCalls"],
        ),
        supports_search_tool: bool_field(
            object,
            &[
                "supports_search_tool",
                "supportsSearchTool",
                "supports_web_search",
                "supportsWebSearch",
            ],
        ),
    })
}

fn merge_missing(existing: &mut DiscoveredModel, candidate: &DiscoveredModel) {
    if existing.display_name.is_none() {
        existing.display_name.clone_from(&candidate.display_name);
    }
    if existing.context_window.is_none() {
        existing.context_window = candidate.context_window;
    }
    if existing.supported_reasoning_levels.is_none() {
        existing
            .supported_reasoning_levels
            .clone_from(&candidate.supported_reasoning_levels);
    }
    if existing.supports_parallel_tool_calls.is_none() {
        existing.supports_parallel_tool_calls = candidate.supports_parallel_tool_calls;
    }
    if existing.supports_search_tool.is_none() {
        existing.supports_search_tool = candidate.supports_search_tool;
    }
}

fn string_field<'a>(object: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str).and_then(non_empty))
}

fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn unsigned_field(object: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_u64))
        .filter(|value| *value > 0)
}

fn bool_field(object: &Map<String, Value>, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_bool))
}

fn reasoning_levels(object: &Map<String, Value>) -> Option<Vec<String>> {
    let value = [
        "supported_reasoning_levels",
        "supportedReasoningLevels",
        "reasoning_levels",
        "reasoningLevels",
    ]
    .iter()
    .find_map(|key| object.get(*key))?;
    let items = value.as_array()?;
    let mut levels = Vec::new();
    for item in items {
        let level = item.as_str().and_then(non_empty).or_else(|| {
            item.as_object()
                .and_then(|entry| string_field(entry, &["effort", "id", "name"]))
        });
        if let Some(level) = level
            && !levels.iter().any(|existing| existing == level)
        {
            levels.push(level.to_owned());
        }
    }
    Some(levels)
}

#[cfg(test)]
mod tests {
    use super::{model_list_url, parse_model_list};

    #[test]
    fn builds_the_protocol_fixed_models_endpoint() {
        assert_eq!(
            model_list_url("https://gateway.example/v1/"),
            "https://gateway.example/v1/models"
        );
    }

    #[test]
    fn parses_common_shapes_and_capability_names_without_defaults() {
        let models = parse_model_list(
            br#"{"data":[{"id":"coder","displayName":"Coder","context_length":128000,"reasoning_levels":[{"effort":"low"},"high"],"supportsParallelToolCalls":true},{"slug":"plain"}]}"#,
        )
        .expect("valid model list");

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "coder");
        assert_eq!(models[0].context_window, Some(128_000));
        assert_eq!(
            models[0].supported_reasoning_levels.as_deref(),
            Some(["low".to_owned(), "high".to_owned()].as_slice())
        );
        assert_eq!(models[0].supports_parallel_tool_calls, Some(true));
        assert_eq!(models[0].supports_search_tool, None);
        assert_eq!(models[1].id, "plain");
        assert_eq!(models[1].context_window, None);
    }

    #[test]
    fn deduplicates_models_and_fills_only_missing_fields() {
        let models = parse_model_list(
            br#"[{"id":"coder","context_window":64000},{"model":"coder","context_window":128000,"supports_search_tool":false}]"#,
        )
        .expect("valid model list");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].context_window, Some(64_000));
        assert_eq!(models[0].supports_search_tool, Some(false));
    }

    #[test]
    fn rejects_a_response_without_models() {
        assert!(parse_model_list(br#"{"data":[]}"#).is_err());
        assert!(parse_model_list(br#"{"object":"list"}"#).is_err());
    }
}
