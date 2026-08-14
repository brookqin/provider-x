use bytes::Bytes;
use serde_json::Value;

use crate::{
    InspectedRequest, ProtocolError,
    inspect::{inspect_object, parse_object},
    rewrite::rewrite_model,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebSocketMessageKind {
    ResponseCreate,
    Other,
}

/// Classifies a Responses WebSocket text message without applying routing validation.
///
/// # Errors
///
/// Returns an error when the message is not valid JSON.
pub fn classify_ws_text(message: &str) -> Result<WebSocketMessageKind, ProtocolError> {
    let value: Value = serde_json::from_str(message)
        .map_err(|error| ProtocolError::InvalidJson(error.to_string()))?;
    Ok(
        if value.get("type").and_then(Value::as_str) == Some("response.create") {
            WebSocketMessageKind::ResponseCreate
        } else {
            WebSocketMessageKind::Other
        },
    )
}

/// Inspects a client `response.create` text message for routing fields.
///
/// # Errors
///
/// Returns an error for malformed JSON, non-`response.create` messages, or an invalid `model`.
pub fn inspect_ws_text(message: &str) -> Result<InspectedRequest, ProtocolError> {
    let object = parse_object(message.as_bytes())?;
    if object.get("type").and_then(Value::as_str) != Some("response.create") {
        return Err(ProtocolError::UnsupportedWebSocketMessage);
    }
    inspect_object(&object)
}

/// Replaces only the top-level `model` field of a client `response.create` message.
///
/// # Errors
///
/// Returns an error when the message is not a valid `response.create` object or the replacement
/// model is invalid.
pub fn rewrite_ws_text(message: &str, upstream_model: &str) -> Result<String, ProtocolError> {
    inspect_ws_text(message)?;
    let bytes: Bytes = rewrite_model(message.as_bytes(), upstream_model)?;
    String::from_utf8(bytes.to_vec())
        .map_err(|error| ProtocolError::Serialization(error.to_string()))
}

/// Returns whether an upstream Responses WebSocket event ends the current generation.
#[must_use]
pub fn is_terminal_ws_event(message: &str) -> bool {
    serde_json::from_str::<Value>(message)
        .ok()
        .and_then(|event| event.get("type").and_then(Value::as_str).map(str::to_owned))
        .is_some_and(|event_type| {
            matches!(
                event_type.as_str(),
                "response.completed" | "response.failed" | "response.incomplete"
            )
        })
}

/// Builds a local Responses WebSocket error event for an ingress or routing failure.
#[must_use]
pub fn websocket_error_event(code: &str, message: &str) -> String {
    serde_json::json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "code": code,
            "message": message,
        }
    })
    .to_string()
}
