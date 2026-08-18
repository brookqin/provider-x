use hyper::{HeaderMap, header, header::HeaderValue};
use provider_x_core::{
    AnthropicThinkingMode, AuthConfig, DiscoveredModel, ProviderKind, TransportConfig,
};

use crate::{
    AuthStyle, HttpResponseAdapter, HttpTarget, PreparedHttpRequest, ProtocolAdapter,
    ProviderDefinition, ProviderError, ProviderProfile, WebSocketPlan,
};

pub const HTTP_ENDPOINT: &str = "https://api.openai.com/v1";
pub const WEBSOCKET_ENDPOINT: &str = "wss://api.openai.com/v1/responses";
pub const MODELS_ENDPOINT: &str = "https://api.openai.com/v1/models";
pub const MODELS_DEV_ID: &str = "openai";

const IMPLEMENTATION_REVISION: u32 = 1;

pub(crate) const DEFINITION: ProviderDefinition = ProviderDefinition {
    kind: ProviderKind::OpenAi,
    display_name: "OpenAI",
    default_namespace: "openai",
    models_dev_id: Some(MODELS_DEV_ID),
    configurable: false,
    protocol: provider_x_core::ProtocolId::OpenaiResponses,
    http_endpoint: HTTP_ENDPOINT,
    websocket_endpoint: Some(WEBSOCKET_ENDPOINT),
    models_endpoint: Some(MODELS_ENDPOINT),
    http_sse: true,
    websocket: true,
};

pub(crate) fn profile() -> ProviderProfile {
    ProviderProfile {
        kind: ProviderKind::OpenAi,
        implementation_id: DEFINITION.default_namespace,
        implementation_revision: IMPLEMENTATION_REVISION,
        adapter: ProtocolAdapter::OpenaiResponses,
        auth_style: AuthStyle::Bearer,
        http_endpoint: HTTP_ENDPOINT.to_owned(),
        websocket_endpoint: Some(WEBSOCKET_ENDPOINT.to_owned()),
        models_endpoint: Some(MODELS_ENDPOINT.to_owned()),
        transports: TransportConfig {
            http_sse: true,
            websocket: true,
        },
        models_dev_id: Some(MODELS_DEV_ID),
        anthropic_thinking: AnthropicThinkingMode::Adaptive,
        credential_scope: None,
        accepts_legacy_fingerprint: false,
    }
}

pub(crate) fn parse_model_list(bytes: &[u8]) -> Result<Vec<DiscoveredModel>, ProviderError> {
    protocol_openai_responses::parse_model_list(bytes)
        .map_err(|error| ProviderError::ModelDiscovery(error.to_string()))
}

pub(crate) fn apply_authentication(
    auth: &AuthConfig,
    headers: &mut HeaderMap,
) -> Result<(), ProviderError> {
    let AuthConfig::Bearer { api_key } = auth else {
        return Err(ProviderError::InvalidAuthenticationHeader);
    };
    let value = HeaderValue::from_str(&format!("Bearer {api_key}"))
        .map_err(|_| ProviderError::InvalidAuthenticationHeader)?;
    headers.insert(header::AUTHORIZATION, value);
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
