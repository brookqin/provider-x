use bytes::Bytes;

use crate::{
    InspectedRequest, ProtocolError, ResponsesPath,
    inspect::{inspect_object, parse_object},
    rewrite::rewrite_model,
};

/// Validates a supported Responses path and extracts the top-level routing fields.
///
/// # Errors
///
/// Returns an error for unsupported paths, malformed JSON objects, or a missing/invalid `model`.
pub fn inspect_http(path: &str, body: &[u8]) -> Result<InspectedRequest, ProtocolError> {
    ResponsesPath::try_from(path)?;
    let object = parse_object(body)?;
    inspect_object(&object)
}

/// Replaces only the top-level `model` field of a Responses JSON body.
///
/// # Errors
///
/// Returns an error when the input is not a valid Responses JSON object or the replacement model
/// is invalid.
pub fn rewrite_http_model(body: &[u8], upstream_model: &str) -> Result<Bytes, ProtocolError> {
    rewrite_model(body, upstream_model)
}

/// Builds the OpenAI-compatible JSON body used for a local HTTP error response.
#[must_use]
pub fn http_error_body(message: &str) -> Bytes {
    Bytes::from(
        serde_json::json!({
            "error": {
                "message": message,
            }
        })
        .to_string(),
    )
}
