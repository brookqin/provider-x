use hyper::{HeaderMap, header, header::HeaderValue};
use provider_x_core::{
    AnthropicThinkingMode, AuthConfig, DiscoveredModel, ProviderConfig, ProviderKind,
    TransportConfig,
};

use crate::{
    AuthStyle, HttpResponseAdapter, HttpTarget, PreparedHttpRequest, ProtocolAdapter,
    ProviderDefinition, ProviderError, ProviderProfile, WebSocketPlan, WsHttpAdapterKind,
};

pub const DEEPSEEK_HTTP_ENDPOINT: &str = "https://api.deepseek.com";
pub const DEEPSEEK_MODELS_ENDPOINT: &str = "https://api.deepseek.com/models";
pub const DEEPSEEK_MODELS_DEV_ID: &str = "deepseek";
pub(crate) const IMPLEMENTATION_REVISION: u32 = 1;

pub(crate) const DEFINITION: ProviderDefinition = ProviderDefinition {
    kind: ProviderKind::DeepSeek,
    display_name: "DeepSeek",
    default_namespace: "deepseek",
    models_dev_id: Some(DEEPSEEK_MODELS_DEV_ID),
    configurable: false,
    protocol: provider_x_core::ProtocolId::OpenaiResponses,
    http_endpoint: DEEPSEEK_HTTP_ENDPOINT,
    models_endpoint: Some(DEEPSEEK_MODELS_ENDPOINT),
    http_sse: true,
    websocket: false,
};

pub(crate) fn profile() -> ProviderProfile {
    ProviderProfile {
        kind: DEFINITION.kind,
        implementation_id: DEFINITION.default_namespace,
        implementation_revision: IMPLEMENTATION_REVISION,
        adapter: ProtocolAdapter::OpenaiResponses,
        auth_style: AuthStyle::Bearer,
        http_endpoint: DEFINITION.http_endpoint.to_owned(),
        websocket_endpoint: None,
        models_endpoint: DEFINITION.models_endpoint.map(str::to_owned),
        transports: TransportConfig {
            http_sse: DEFINITION.http_sse,
            websocket: DEFINITION.websocket,
        },
        models_dev_id: DEFINITION.models_dev_id,
        anthropic_thinking: AnthropicThinkingMode::Adaptive,
        accepts_legacy_fingerprint: true,
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
    let AuthConfig::Bearer { api_key } = auth;
    let value = HeaderValue::from_str(&format!("Bearer {api_key}"))
        .map_err(|_| ProviderError::InvalidAuthenticationHeader)?;
    headers.insert(header::AUTHORIZATION, value);
    Ok(())
}

pub(crate) fn prepare_http_request(
    body: &[u8],
    upstream_model: &str,
    _max_bytes: usize,
) -> Result<PreparedHttpRequest, ProviderError> {
    Ok(PreparedHttpRequest {
        target: HttpTarget::PreserveIngressPath(DEEPSEEK_HTTP_ENDPOINT.to_owned()),
        body: protocol_openai_responses::rewrite_http_model(body, upstream_model)
            .map_err(|error| ProviderError::RequestConversion(error.to_string()))?,
        response_adapter: HttpResponseAdapter::Passthrough,
    })
}

pub(crate) const fn websocket_plan() -> WebSocketPlan {
    WebSocketPlan::HttpBridge(WsHttpAdapterKind::OpenaiResponses)
}

pub(crate) fn websocket_http_url() -> String {
    format!("{DEEPSEEK_HTTP_ENDPOINT}/v1/responses")
}

pub(crate) fn legacy_routing_fingerprint(
    provider: &ProviderConfig,
) -> Result<String, provider_x_core::CoreError> {
    let mut legacy = provider.clone();
    legacy.endpoints.models = None;
    legacy.routing_fingerprint()
}

pub(crate) fn rewrite_websocket_request(
    message: &str,
    upstream_model: &str,
) -> Result<String, ProviderError> {
    protocol_openai_responses::rewrite_ws_text(message, upstream_model)
        .map_err(|error| ProviderError::RequestConversion(error.to_string()))
}
