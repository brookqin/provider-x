mod auth;

use hyper::{HeaderMap, header, header::HeaderValue};
use provider_x_core::{
    AnthropicThinkingMode, AuthConfig, DiscoveredModel, ProviderKind, TransportConfig,
};
use sha2::{Digest as _, Sha256};

use crate::{
    AuthStyle, HttpResponseAdapter, HttpTarget, PreparedHttpRequest, ProtocolAdapter,
    ProviderDefinition, ProviderError, ProviderProfile, WebSocketPlan,
};

pub use auth::{OpenAiOAuthClient, OpenAiOAuthError, needs_refresh};

pub const HTTP_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex";
pub const WEBSOCKET_ENDPOINT: &str = "wss://chatgpt.com/backend-api/codex/responses";
pub const MODELS_ENDPOINT: &str = concat!(
    "https://chatgpt.com/backend-api/codex/models?client_version=",
    env!("CARGO_PKG_VERSION")
);
pub const MODELS_DEV_ID: &str = "openai";

const IMPLEMENTATION_REVISION: u32 = 1;

pub(crate) const DEFINITION: ProviderDefinition = ProviderDefinition {
    kind: ProviderKind::OpenAiOAuth,
    display_name: "OpenAI (OAuth)",
    default_namespace: "openai-oauth",
    models_dev_id: Some(MODELS_DEV_ID),
    configurable: false,
    protocol: provider_x_core::ProtocolId::OpenaiResponses,
    http_endpoint: HTTP_ENDPOINT,
    websocket_endpoint: Some(WEBSOCKET_ENDPOINT),
    models_endpoint: Some(MODELS_ENDPOINT),
    http_sse: true,
    websocket: true,
};

pub(crate) fn profile(auth: &AuthConfig) -> ProviderProfile {
    ProviderProfile {
        kind: ProviderKind::OpenAiOAuth,
        implementation_id: DEFINITION.default_namespace,
        implementation_revision: IMPLEMENTATION_REVISION,
        adapter: ProtocolAdapter::OpenaiResponses,
        auth_style: AuthStyle::OpenAiOAuth,
        http_endpoint: HTTP_ENDPOINT.to_owned(),
        websocket_endpoint: Some(WEBSOCKET_ENDPOINT.to_owned()),
        models_endpoint: Some(MODELS_ENDPOINT.to_owned()),
        transports: TransportConfig {
            http_sse: true,
            websocket: true,
        },
        models_dev_id: Some(MODELS_DEV_ID),
        anthropic_thinking: AnthropicThinkingMode::Adaptive,
        credential_scope: account_scope(auth),
        accepts_legacy_fingerprint: false,
    }
}

fn account_scope(auth: &AuthConfig) -> Option<String> {
    let AuthConfig::OpenAiOAuth { account_id, .. } = auth else {
        return None;
    };
    Some(format!(
        "sha256:{}",
        hex::encode(Sha256::digest(account_id.as_bytes()))
    ))
}

pub(crate) fn parse_model_list(bytes: &[u8]) -> Result<Vec<DiscoveredModel>, ProviderError> {
    protocol_openai_responses::parse_model_list(bytes)
        .map_err(|error| ProviderError::ModelDiscovery(error.to_string()))
}

pub(crate) fn apply_authentication(
    auth: &AuthConfig,
    headers: &mut HeaderMap,
) -> Result<(), ProviderError> {
    let AuthConfig::OpenAiOAuth {
        access_token,
        account_id,
        is_fedramp,
        ..
    } = auth
    else {
        return Err(ProviderError::InvalidAuthenticationHeader);
    };
    let authorization = HeaderValue::from_str(&format!("Bearer {access_token}"))
        .map_err(|_| ProviderError::InvalidAuthenticationHeader)?;
    let account_id = HeaderValue::from_str(account_id)
        .map_err(|_| ProviderError::InvalidAuthenticationHeader)?;
    headers.insert(header::AUTHORIZATION, authorization);
    headers.insert("chatgpt-account-id", account_id);
    if *is_fedramp {
        headers.insert("x-openai-fedramp", HeaderValue::from_static("true"));
    }
    headers
        .entry("version")
        .or_insert(HeaderValue::from_static(env!("CARGO_PKG_VERSION")));
    Ok(())
}

pub(crate) fn prepare_http_request(
    body: &[u8],
    upstream_model: &str,
) -> Result<PreparedHttpRequest, ProviderError> {
    Ok(PreparedHttpRequest {
        target: HttpTarget::PreserveIngressPath(HTTP_ENDPOINT.to_owned()),
        body: protocol_openai_responses::rewrite_http_model(body, upstream_model)
            .map_err(|error| ProviderError::RequestConversion(error.to_string()))?,
        response_adapter: HttpResponseAdapter::Passthrough,
    })
}

pub(crate) const fn websocket_plan() -> WebSocketPlan {
    WebSocketPlan::Direct
}

pub(crate) fn websocket_http_url() -> String {
    protocol_openai_responses::responses_url(HTTP_ENDPOINT)
}

pub(crate) fn rewrite_websocket_request(
    message: &str,
    upstream_model: &str,
) -> Result<String, ProviderError> {
    protocol_openai_responses::rewrite_ws_text(message, upstream_model)
        .map_err(|error| ProviderError::RequestConversion(error.to_string()))
}
