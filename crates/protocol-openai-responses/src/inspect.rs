use serde_json::{Map, Value};

use crate::ProtocolError;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StandardMetadata {
    pub client_metadata: Option<Value>,
    pub previous_response_id_present: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InspectedRequest {
    pub model: String,
    pub metadata: StandardMetadata,
}

pub(crate) fn parse_object(input: &[u8]) -> Result<Map<String, Value>, ProtocolError> {
    let value: Value = serde_json::from_slice(input)
        .map_err(|error| ProtocolError::InvalidJson(error.to_string()))?;
    value
        .as_object()
        .cloned()
        .ok_or(ProtocolError::BodyMustBeObject)
}

pub(crate) fn inspect_object(
    object: &Map<String, Value>,
) -> Result<InspectedRequest, ProtocolError> {
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty() && model.trim() == *model)
        .ok_or(ProtocolError::InvalidModel)?;

    Ok(InspectedRequest {
        model: model.to_owned(),
        metadata: StandardMetadata {
            client_metadata: object.get("client_metadata").cloned(),
            previous_response_id_present: object
                .get("previous_response_id")
                .is_some_and(|value| !value.is_null()),
        },
    })
}
