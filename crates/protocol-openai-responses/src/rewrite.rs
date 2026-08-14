use bytes::Bytes;
use serde_json::Value;

use crate::{ProtocolError, inspect::parse_object};

pub(crate) fn rewrite_model(input: &[u8], upstream_model: &str) -> Result<Bytes, ProtocolError> {
    if upstream_model.is_empty() || upstream_model.trim() != upstream_model {
        return Err(ProtocolError::InvalidModel);
    }

    let mut object = parse_object(input)?;
    if !matches!(object.get("model"), Some(Value::String(model)) if !model.is_empty()) {
        return Err(ProtocolError::InvalidModel);
    }
    object.insert("model".to_owned(), Value::String(upstream_model.to_owned()));
    serde_json::to_vec(&Value::Object(object))
        .map(Bytes::from)
        .map_err(|error| ProtocolError::Serialization(error.to_string()))
}
