use hyper::HeaderMap;
use provider_x_core::{AuthConfig, DiscoveredModel, ProviderConfig, ProviderKind};

use crate::{
    AuthStyle, PreparedHttpRequest, ProtocolAdapter, ProviderDefinition, ProviderError,
    ProviderProfile, WebSocketPlan,
};

const IMPLEMENTATION_REVISION: u32 = 1;

pub(crate) const DEFINITION: ProviderDefinition = ProviderDefinition {
    kind: ProviderKind::Custom,
    display_name: "Custom",
    default_namespace: "custom",
    models_dev_id: None,
    configurable: true,
    protocol: provider_x_core::ProtocolId::OpenaiResponses,
    http_endpoint: "",
    models_endpoint: None,
    http_sse: true,
    websocket: false,
};

pub(crate) fn profile(provider: &ProviderConfig) -> ProviderProfile {
    let adapter = ProtocolAdapter::from(provider.protocol);
    ProviderProfile {
        kind: DEFINITION.kind,
        implementation_id: DEFINITION.default_namespace,
        implementation_revision: IMPLEMENTATION_REVISION,
        adapter,
        auth_style: if adapter == ProtocolAdapter::AnthropicMessages {
            AuthStyle::Anthropic
        } else {
            AuthStyle::Bearer
        },
        http_endpoint: provider.endpoints.http.clone(),
        websocket_endpoint: provider.endpoints.websocket.clone(),
        models_endpoint: provider.endpoints.models.clone(),
        transports: provider.transports.clone(),
        models_dev_id: DEFINITION.models_dev_id,
        anthropic_thinking: provider.anthropic_thinking_mode(),
        accepts_legacy_fingerprint: true,
    }
}

pub(crate) fn parse_model_list(
    profile: &ProviderProfile,
    bytes: &[u8],
) -> Result<Vec<DiscoveredModel>, ProviderError> {
    profile.parse_model_list_by_adapter(bytes)
}

pub(crate) fn apply_authentication(
    profile: &ProviderProfile,
    auth: &AuthConfig,
    headers: &mut HeaderMap,
) -> Result<(), ProviderError> {
    profile.apply_authentication_by_style(auth, headers)
}

pub(crate) fn prepare_http_request(
    profile: &ProviderProfile,
    body: &[u8],
    upstream_model: &str,
    max_bytes: usize,
) -> Result<PreparedHttpRequest, ProviderError> {
    profile.prepare_http_request_by_adapter(body, upstream_model, max_bytes)
}

pub(crate) const fn websocket_plan(profile: &ProviderProfile) -> WebSocketPlan {
    profile.websocket_plan_by_adapter()
}

pub(crate) fn websocket_http_url(profile: &ProviderProfile) -> String {
    match profile.adapter {
        ProtocolAdapter::OpenaiResponses => {
            protocol_openai_responses::responses_url(&profile.http_endpoint)
        }
        ProtocolAdapter::OpenaiChatCompletions => {
            protocol_openai_chat_completions::chat_completions_url(&profile.http_endpoint)
        }
        ProtocolAdapter::AnthropicMessages => {
            protocol_anthropic_messages::messages_url(&profile.http_endpoint)
        }
    }
}

pub(crate) fn rewrite_websocket_request(
    profile: &ProviderProfile,
    message: &str,
    upstream_model: &str,
) -> Result<String, ProviderError> {
    profile.rewrite_websocket_request_by_adapter(message, upstream_model)
}
