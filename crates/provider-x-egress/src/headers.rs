use std::collections::BTreeSet;

use hyper::{HeaderMap, header, header::HeaderValue};
use provider_x_core::AuthConfig;

use crate::ProxyError;

pub(crate) fn official_request_headers(source: &HeaderMap) -> HeaderMap {
    let connection_headers = connection_headers(source);
    source
        .iter()
        .filter(|(name, _)| {
            !is_hop_by_hop(name.as_str())
                && !connection_headers.contains(name.as_str())
                && *name != header::HOST
                && *name != header::CONTENT_LENGTH
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// Official catalog requests are the only GET requests whose body provider-x may rewrite.
/// Request identity encoding so the upstream JSON can be parsed without implementing an
/// unrelated content-decoding policy. Authentication remains identical to other official calls.
pub(crate) fn official_model_catalog_headers(source: &HeaderMap) -> HeaderMap {
    let mut headers = official_request_headers(source);
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    headers
}

pub(crate) fn third_party_request_headers(
    source: &HeaderMap,
    auth: &AuthConfig,
) -> Result<HeaderMap, ProxyError> {
    let connection_headers = connection_headers(source);
    let mut destination: HeaderMap = source
        .iter()
        .filter(|(name, _)| {
            let name = name.as_str();
            !is_hop_by_hop(name)
                && !connection_headers.contains(name)
                && !is_sensitive_official_header(name)
                // Codex may use zstd for its built-in official transport. A generic Responses
                // Provider is only required to accept JSON, so the third-party path always
                // forwards the already-decoded and rewritten identity body.
                && !matches!(name, "host" | "content-length" | "content-encoding")
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();

    match auth {
        AuthConfig::Bearer { api_key } => {
            let value = format!("Bearer {api_key}");
            let value = HeaderValue::from_str(&value).map_err(|_| ProxyError::RequestBuild)?;
            destination.insert(header::AUTHORIZATION, value);
        }
    }
    Ok(destination)
}

pub(crate) fn response_headers(source: &HeaderMap) -> HeaderMap {
    let connection_headers = connection_headers(source);
    source
        .iter()
        .filter(|(name, _)| {
            !is_hop_by_hop(name.as_str()) && !connection_headers.contains(name.as_str())
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

pub(crate) fn rewritten_response_headers(source: &HeaderMap) -> HeaderMap {
    let mut headers = response_headers(source);
    for name in [
        header::CONTENT_LENGTH,
        header::CONTENT_ENCODING,
        header::ETAG,
        header::LAST_MODIFIED,
    ] {
        headers.remove(name);
    }
    headers.remove("content-md5");
    headers.remove("digest");
    headers
}

pub(crate) fn official_websocket_headers(source: &HeaderMap) -> HeaderMap {
    let mut headers = official_request_headers(source);
    remove_websocket_handshake_headers(&mut headers);
    headers
}

pub(crate) fn third_party_websocket_headers(
    source: &HeaderMap,
    auth: &AuthConfig,
) -> Result<HeaderMap, ProxyError> {
    let mut headers = third_party_request_headers(source, auth)?;
    remove_websocket_handshake_headers(&mut headers);
    Ok(headers)
}

fn remove_websocket_handshake_headers(headers: &mut HeaderMap) {
    let names: Vec<_> = headers
        .keys()
        .filter(|name| name.as_str().starts_with("sec-websocket-"))
        .cloned()
        .collect();
    for name in names {
        headers.remove(name);
    }
}

fn connection_headers(source: &HeaderMap) -> BTreeSet<String> {
    source
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn is_sensitive_official_header(name: &str) -> bool {
    name == "authorization"
        || name == "cookie"
        || name.starts_with("chatgpt-")
        || name.starts_with("x-openai-")
        || matches!(name, "openai-organization" | "openai-project")
        || name.contains("attestation")
        || name.contains("fedramp")
}

#[cfg(test)]
mod tests {
    use hyper::{HeaderMap, header::HeaderValue};
    use provider_x_core::AuthConfig;

    use super::{
        official_model_catalog_headers, official_request_headers, rewritten_response_headers,
        third_party_request_headers,
    };

    #[test]
    fn official_preserves_auth_but_removes_hop_by_hop_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer official"));
        headers.insert(
            "connection",
            HeaderValue::from_static("keep-alive, x-local"),
        );
        headers.insert("x-local", HeaderValue::from_static("remove-me"));
        let result = official_request_headers(&headers);
        assert_eq!(result["authorization"], "Bearer official");
        assert!(!result.contains_key("connection"));
        assert!(!result.contains_key("x-local"));
    }

    #[test]
    fn third_party_replaces_all_official_credentials() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer official"));
        headers.insert("chatgpt-account-id", HeaderValue::from_static("account"));
        headers.insert("x-openai-attestation", HeaderValue::from_static("proof"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert("content-encoding", HeaderValue::from_static("zstd"));
        let result = third_party_request_headers(
            &headers,
            &AuthConfig::Bearer {
                api_key: "third-party".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(result["authorization"], "Bearer third-party");
        assert_eq!(result["content-type"], "application/json");
        assert!(!result.contains_key("content-encoding"));
        assert!(!result.contains_key("chatgpt-account-id"));
        assert!(!result.contains_key("x-openai-attestation"));
    }

    #[test]
    fn catalog_rewrite_preserves_official_auth_but_replaces_representation_metadata() {
        let mut request = HeaderMap::new();
        request.insert("authorization", HeaderValue::from_static("Bearer official"));
        request.insert("accept-encoding", HeaderValue::from_static("zstd"));
        let request = official_model_catalog_headers(&request);
        assert_eq!(request["authorization"], "Bearer official");
        assert_eq!(request["accept-encoding"], "identity");

        let mut response = HeaderMap::new();
        response.insert("content-length", HeaderValue::from_static("12"));
        response.insert("content-encoding", HeaderValue::from_static("gzip"));
        response.insert("etag", HeaderValue::from_static("original"));
        response.insert("x-request-id", HeaderValue::from_static("kept"));
        let response = rewritten_response_headers(&response);
        assert!(!response.contains_key("content-length"));
        assert!(!response.contains_key("content-encoding"));
        assert!(!response.contains_key("etag"));
        assert_eq!(response["x-request-id"], "kept");
    }
}
