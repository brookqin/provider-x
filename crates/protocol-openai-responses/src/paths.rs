use crate::ProtocolError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponsesPath {
    Create,
    Compact,
}

#[must_use]
pub fn responses_url(base: &str) -> String {
    format!("{}/responses", base.trim_end_matches('/'))
}

impl TryFrom<&str> for ResponsesPath {
    type Error = ProtocolError;

    fn try_from(path: &str) -> Result<Self, Self::Error> {
        match path {
            "/v1/responses" => Ok(Self::Create),
            "/v1/responses/compact" => Ok(Self::Compact),
            _ => Err(ProtocolError::UnsupportedPath(path.to_owned())),
        }
    }
}
