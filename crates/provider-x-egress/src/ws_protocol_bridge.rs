use protocol_anthropic_messages::AnthropicMessagesWsHttpAdapter;
use protocol_openai_chat_completions::ChatCompletionsWsHttpAdapter;
use protocol_openai_responses::ResponsesWsHttpAdapter;
use provider_x_core::ProtocolId;
use tokio::sync::watch;

use crate::{
    EgressState, ObservedRoute,
    state::ProviderEgress,
    ws_http_runner::WsHttpSessionContext,
    ws_proxy::{DownstreamSocket, WebSocketProxyError, WebSocketShutdown},
};

#[allow(clippy::too_many_arguments)] // Dispatch preserves one immutable routed session context.
pub(crate) async fn run(
    downstream: &mut DownstreamSocket,
    provider: &ProviderEgress,
    runtime: &crate::state::EgressRuntimeSnapshot,
    request_headers: &hyper::HeaderMap,
    first_text: &str,
    upstream_model: String,
    observed_route: ObservedRoute,
    codex_turn_metadata_header_present: bool,
    state: &EgressState,
    shutdown: &mut watch::Receiver<WebSocketShutdown>,
) -> Result<(), WebSocketProxyError> {
    match provider.config.protocol {
        ProtocolId::OpenaiResponses => {
            crate::ws_http_runner::run::<ResponsesWsHttpAdapter>(
                downstream,
                WsHttpSessionContext {
                    provider,
                    runtime,
                    request_headers,
                    first_text,
                    upstream_model,
                    observed_route,
                    codex_turn_metadata_header_present,
                    state,
                },
                shutdown,
            )
            .await
        }
        ProtocolId::OpenaiChatCompletions => {
            crate::ws_http_runner::run::<ChatCompletionsWsHttpAdapter>(
                downstream,
                WsHttpSessionContext {
                    provider,
                    runtime,
                    request_headers,
                    first_text,
                    upstream_model,
                    observed_route,
                    codex_turn_metadata_header_present,
                    state,
                },
                shutdown,
            )
            .await
        }
        ProtocolId::AnthropicMessages => {
            let adapter = AnthropicMessagesWsHttpAdapter::new_session_with_thinking_mode(
                upstream_model.clone(),
                state.request_body_limit_bytes,
                provider.config.anthropic_thinking_mode(),
            );
            crate::ws_http_runner::run_with_adapter::<AnthropicMessagesWsHttpAdapter>(
                downstream,
                WsHttpSessionContext {
                    provider,
                    runtime,
                    request_headers,
                    first_text,
                    upstream_model,
                    observed_route,
                    codex_turn_metadata_header_present,
                    state,
                },
                adapter,
                shutdown,
            )
            .await
        }
    }
}
