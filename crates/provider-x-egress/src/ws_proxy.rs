use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Empty};
use hyper::{
    Method, Request, Response, StatusCode, Version,
    body::Incoming,
    header::{
        CONNECTION, HeaderMap, HeaderValue, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY,
        SEC_WEBSOCKET_PROTOCOL, SEC_WEBSOCKET_VERSION, UPGRADE,
    },
    upgrade::Upgraded,
};
use hyper_util::rt::TokioIo;
use protocol_openai_responses::{
    WebSocketMessageKind, classify_ws_text, inspect_ws_text, is_terminal_ws_event,
    websocket_error_event,
};
use provider_x_core::{ProviderId, RouteDecision};
use provider_x_network::{NetworkConnector, NetworkWebSocket, connect_websocket};
use provider_x_providers::WebSocketPlan;
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, watch};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        client::IntoClientRequest,
        handshake::derive_accept_key,
        protocol::{CloseFrame, Message, Role, WebSocketConfig, frame::coding::CloseCode},
    },
};
use tokio_util::task::TaskTracker;

use crate::{
    EgressEvent, EgressState, ErrorObserved, ObservedRoute, ObservedTransport, ProxyError,
    RequestObserved, UpstreamObserved,
    headers::{official_websocket_headers, third_party_websocket_headers},
    server::ProxyBody,
};

pub(crate) type DownstreamSocket = WebSocketStream<TokioIo<Upgraded>>;
type UpstreamSocket = NetworkWebSocket;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WebSocketShutdown {
    Running,
    Draining,
    Force,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SessionRoute {
    Official,
    ThirdParty(ProviderId),
}

struct PreparedFirstMessage {
    runtime: Arc<crate::state::EgressRuntimeSnapshot>,
    route: SessionRoute,
    observed_route: ObservedRoute,
    codex_turn_metadata_header_present: bool,
    upstream: PreparedUpstream,
}

enum PreparedUpstream {
    WebSocket {
        url: String,
        headers: HeaderMap,
        message: Message,
        connector: NetworkConnector,
    },
    HttpBridge {
        provider: Box<crate::state::ProviderEgress>,
        upstream_model: String,
        first_text: String,
    },
}

#[derive(Debug, Error)]
pub(crate) enum WebSocketProxyError {
    #[error("the first WebSocket message must be a valid response.create")]
    InvalidFirstMessage,

    #[error("the selected model is not available")]
    ModelNotAvailable,

    #[error("the selected Provider is not available")]
    ProviderNotAvailable,

    #[error("the selected Provider does not support WebSocket")]
    TransportNotSupported,

    #[error("a WebSocket connection cannot switch Provider")]
    RouteChanged,

    #[error("failed to connect to the upstream WebSocket")]
    UpstreamConnect,

    #[error("WebSocket connection was idle for too long")]
    IdleTimeout,

    #[error("WebSocket connection is shutting down")]
    Shutdown,

    #[error("WebSocket transport failed")]
    Transport,

    #[error("the client closed the WebSocket")]
    ClientClosed,

    #[error("only one upstream response may be in flight per WebSocket")]
    ConcurrentRequest,

    #[error("the upstream HTTP endpoint returned status {0}")]
    UpstreamStatus(StatusCode),

    #[error("the upstream HTTP stream ended without a terminal event")]
    InvalidUpstreamStream,

    #[error("the WebSocket-to-HTTP session exceeded its bounded history limit")]
    SessionHistoryLimit,
}

impl WebSocketProxyError {
    const fn log_code(&self) -> &'static str {
        match self {
            Self::TransportNotSupported => "transport_not_supported",
            Self::ModelNotAvailable => "model_not_available",
            Self::RouteChanged => "route_changed",
            Self::IdleTimeout => "idle_timeout",
            Self::Shutdown => "service_restart",
            Self::SessionHistoryLimit => "session_history_limit",
            Self::ConcurrentRequest => "concurrent_request",
            Self::InvalidFirstMessage => "invalid_request",
            Self::ProviderNotAvailable => "provider_not_available",
            Self::UpstreamConnect => "upstream_connect_failed",
            Self::UpstreamStatus(_) => "upstream_http_status",
            Self::InvalidUpstreamStream => "invalid_upstream_stream",
            Self::ClientClosed => "client_closed",
            Self::Transport => "websocket_transport_failed",
        }
    }

    const fn status(&self) -> Option<u16> {
        match self {
            Self::UpstreamStatus(status) => Some(status.as_u16()),
            _ => None,
        }
    }

    const fn client_code(&self) -> &'static str {
        match self {
            Self::TransportNotSupported => "transport_not_supported",
            Self::ModelNotAvailable => "model_not_available",
            Self::RouteChanged => "route_changed",
            Self::IdleTimeout => "idle_timeout",
            Self::Shutdown => "service_restart",
            Self::SessionHistoryLimit => "session_history_limit",
            Self::ConcurrentRequest => "concurrent_request",
            Self::InvalidFirstMessage => "invalid_request",
            Self::ProviderNotAvailable
            | Self::UpstreamConnect
            | Self::UpstreamStatus(_)
            | Self::InvalidUpstreamStream
            | Self::ClientClosed
            | Self::Transport => "upstream_error",
        }
    }
}

pub(crate) fn is_websocket_upgrade(request: &Request<Incoming>) -> bool {
    request
        .headers()
        .get(UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

pub(crate) fn websocket_upgrade(
    mut request: Request<Incoming>,
    state: Arc<EgressState>,
    tasks: &TaskTracker,
    shutdown: watch::Receiver<WebSocketShutdown>,
    connection_permit: Arc<OwnedSemaphorePermit>,
) -> Result<Response<ProxyBody>, ProxyError> {
    validate_handshake(&request)?;
    let key = request
        .headers()
        .get(SEC_WEBSOCKET_KEY)
        .ok_or(ProxyError::InvalidWebSocketHandshake)?;
    let accept = derive_accept_key(key.as_bytes());
    let request_headers = request.headers().clone();
    let on_upgrade = hyper::upgrade::on(&mut request);
    tasks.spawn(async move {
        let _connection_permit = connection_permit;
        let upgraded = tokio::select! {
            () = wait_for_drain(shutdown.clone()) => return,
            upgraded = on_upgrade => match upgraded {
                Ok(upgraded) => upgraded,
                Err(_) => return,
            },
        };
        let socket = WebSocketStream::from_raw_socket(
            TokioIo::new(upgraded),
            Role::Server,
            Some(websocket_config(state.request_body_limit_bytes)),
        )
        .await;
        run_session(socket, request_headers, state, shutdown).await;
    });

    let mut response = Response::new(
        Empty::<Bytes>::new()
            .map_err(|never| match never {})
            .boxed(),
    );
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    *response.version_mut() = Version::HTTP_11;
    response
        .headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("Upgrade"));
    response
        .headers_mut()
        .insert(UPGRADE, HeaderValue::from_static("websocket"));
    response.headers_mut().insert(
        SEC_WEBSOCKET_ACCEPT,
        HeaderValue::from_str(&accept).map_err(|_| ProxyError::InvalidWebSocketHandshake)?,
    );
    Ok(response)
}

fn validate_handshake(request: &Request<Incoming>) -> Result<(), ProxyError> {
    let connection_has_upgrade = request
        .headers()
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    let valid = request.method() == Method::GET
        && request.version() >= Version::HTTP_11
        && request.uri().path() == "/v1/responses"
        && connection_has_upgrade
        && request.headers().get(SEC_WEBSOCKET_KEY).is_some()
        && request
            .headers()
            .get(SEC_WEBSOCKET_VERSION)
            .is_some_and(|value| value == "13")
        && !request.headers().contains_key(SEC_WEBSOCKET_PROTOCOL);
    if valid {
        Ok(())
    } else {
        Err(ProxyError::InvalidWebSocketHandshake)
    }
}

async fn run_session(
    mut downstream: DownstreamSocket,
    request_headers: HeaderMap,
    state: Arc<EgressState>,
    mut shutdown: watch::Receiver<WebSocketShutdown>,
) {
    let result = async {
        let first = receive_first_message(&mut downstream, &state, &mut shutdown).await?;
        let prepared = prepare_first_message(first, &request_headers, &state)?;
        let PreparedFirstMessage {
            runtime,
            route,
            observed_route,
            codex_turn_metadata_header_present,
            upstream,
        } = prepared;
        let PreparedUpstream::WebSocket {
            url,
            headers,
            message,
            connector,
        } = upstream
        else {
            let PreparedUpstream::HttpBridge {
                provider,
                upstream_model,
                first_text,
            } = upstream
            else {
                unreachable!()
            };
            return crate::ws_protocol_bridge::run(
                &mut downstream,
                &provider,
                &runtime,
                &request_headers,
                &first_text,
                upstream_model,
                observed_route,
                codex_turn_metadata_header_present,
                &state,
                &mut shutdown,
            )
            .await;
        };
        let mut upstream_request = url
            .as_str()
            .into_client_request()
            .map_err(|_| WebSocketProxyError::UpstreamConnect)?;
        for (name, value) in &headers {
            upstream_request
                .headers_mut()
                .append(name.clone(), value.clone());
        }
        let (mut upstream, upstream_response) = tokio::select! {
            () = wait_for_force(shutdown.clone()) => return Err(WebSocketProxyError::Shutdown),
            result = tokio::time::timeout(
                Duration::from_millis(state.response_headers_timeout_ms),
                connect_websocket(
                    connector,
                    upstream_request,
                    Some(websocket_config(state.request_body_limit_bytes)),
                ),
            ) => result
                .map_err(|_| WebSocketProxyError::UpstreamConnect)?
                .map_err(|_| WebSocketProxyError::UpstreamConnect)?,
        };
        state.observe(EgressEvent::UpstreamObserved(UpstreamObserved {
            transport: ObservedTransport::WebSocket,
            route: observed_route,
            status: upstream_response.status().as_u16(),
        }));
        send_upstream(
            &mut upstream,
            message,
            state.websocket_idle_timeout_ms,
            shutdown.clone(),
        )
        .await?;
        let relay_result = relay(
            &mut downstream,
            &mut upstream,
            runtime,
            route,
            codex_turn_metadata_header_present,
            &state,
            &mut shutdown,
        )
        .await;
        if relay_result.is_err() {
            let _ = upstream
                .send(close_message(CloseCode::Policy, "proxy error"))
                .await;
        }
        relay_result
    }
    .await;

    if let Err(error) = result
        && is_reportable_session_error(&error)
    {
        observe_session_error(&state, &error);
        reject_downstream(&mut downstream, &error).await;
    }
}

fn is_reportable_session_error(error: &WebSocketProxyError) -> bool {
    !matches!(
        error,
        WebSocketProxyError::ClientClosed | WebSocketProxyError::Shutdown
    )
}

fn observe_session_error(state: &EgressState, error: &WebSocketProxyError) {
    state.observe(EgressEvent::ErrorObserved(ErrorObserved {
        transport: ObservedTransport::WebSocket,
        method: "GET".to_owned(),
        path: "/v1/responses".to_owned(),
        ingress_authorized: true,
        status: error.status(),
        code: error.log_code().to_owned(),
        message: error.to_string(),
    }));
}

async fn receive_first_message(
    downstream: &mut DownstreamSocket,
    state: &EgressState,
    shutdown: &mut watch::Receiver<WebSocketShutdown>,
) -> Result<Message, WebSocketProxyError> {
    loop {
        let message = tokio::select! {
            biased;
            () = wait_for_drain(shutdown.clone()) => return Err(WebSocketProxyError::Shutdown),
            message = tokio::time::timeout(
                Duration::from_millis(state.websocket_idle_timeout_ms),
                downstream.next(),
            ) => message
                .map_err(|_| WebSocketProxyError::IdleTimeout)?
                .ok_or(WebSocketProxyError::Transport)?
                .map_err(|_| WebSocketProxyError::Transport)?,
        };
        match message {
            Message::Text(text) => {
                inspect_ws_text(text.as_ref())
                    .map_err(|_| WebSocketProxyError::InvalidFirstMessage)?;
                return Ok(Message::Text(text));
            }
            Message::Ping(_) | Message::Pong(_) => {
                downstream
                    .flush()
                    .await
                    .map_err(|_| WebSocketProxyError::Transport)?;
            }
            Message::Close(_) => return Err(WebSocketProxyError::Transport),
            Message::Binary(_) | Message::Frame(_) => {
                return Err(WebSocketProxyError::InvalidFirstMessage);
            }
        }
    }
}

#[allow(clippy::too_many_lines)] // The first message freezes route and transport atomically.
fn prepare_first_message(
    message: Message,
    request_headers: &HeaderMap,
    state: &EgressState,
) -> Result<PreparedFirstMessage, WebSocketProxyError> {
    let Message::Text(text) = message else {
        return Err(WebSocketProxyError::InvalidFirstMessage);
    };
    let inspected =
        inspect_ws_text(text.as_ref()).map_err(|_| WebSocketProxyError::InvalidFirstMessage)?;
    let runtime = state.runtime.load_full();
    let decision = runtime.resolve(&inspected.model);
    let observed_route = ObservedRoute::from(&decision);
    let codex_turn_metadata_header_present = request_headers.contains_key("x-codex-turn-metadata");
    state.observe(EgressEvent::RequestObserved(RequestObserved {
        transport: ObservedTransport::WebSocket,
        path: "/v1/responses".to_owned(),
        sequence: 1,
        model: inspected.model.clone(),
        route: observed_route.clone(),
        previous_response_id_present: inspected.metadata.previous_response_id_present,
        client_metadata_present: inspected.metadata.client_metadata.is_some(),
        codex_turn_metadata_header_present,
    }));
    match decision {
        RouteDecision::BuiltInOfficial => Ok(PreparedFirstMessage {
            runtime: Arc::clone(&runtime),
            route: SessionRoute::Official,
            observed_route,
            codex_turn_metadata_header_present,
            upstream: PreparedUpstream::WebSocket {
                url: official_websocket_url(&state.official_base_url)
                    .ok_or(WebSocketProxyError::ProviderNotAvailable)?,
                headers: official_websocket_headers(request_headers),
                message: Message::Text(text),
                connector: state.official_websocket_connector.clone(),
            },
        }),
        RouteDecision::UnavailableManagedModel => Err(WebSocketProxyError::ModelNotAvailable),
        RouteDecision::ThirdParty {
            provider_id,
            upstream_model_id,
        } => {
            let provider = runtime
                .provider(&provider_id)
                .ok_or(WebSocketProxyError::ProviderNotAvailable)?;
            match provider.profile.websocket_plan() {
                WebSocketPlan::HttpBridge(_) => Ok(PreparedFirstMessage {
                    runtime: Arc::clone(&runtime),
                    route: SessionRoute::ThirdParty(provider_id),
                    observed_route,
                    codex_turn_metadata_header_present,
                    upstream: PreparedUpstream::HttpBridge {
                        provider: Box::new(provider.clone()),
                        upstream_model: upstream_model_id.to_string(),
                        first_text: text.to_string(),
                    },
                }),
                WebSocketPlan::Direct => {
                    let upstream_url = provider
                        .profile
                        .websocket_endpoint()
                        .map(str::to_owned)
                        .ok_or(WebSocketProxyError::TransportNotSupported)?;
                    let rewritten = provider
                        .profile
                        .rewrite_websocket_request(text.as_ref(), upstream_model_id.as_str())
                        .map_err(|_| WebSocketProxyError::InvalidFirstMessage)?;
                    Ok(PreparedFirstMessage {
                        runtime: Arc::clone(&runtime),
                        route: SessionRoute::ThirdParty(provider_id),
                        observed_route,
                        codex_turn_metadata_header_present,
                        upstream: PreparedUpstream::WebSocket {
                            url: upstream_url,
                            headers: third_party_websocket_headers(
                                request_headers,
                                &provider.config.auth,
                                &provider.profile,
                            )
                            .map_err(|_| WebSocketProxyError::ProviderNotAvailable)?,
                            message: Message::text(rewritten),
                            connector: provider.websocket_connector.clone(),
                        },
                    })
                }
                WebSocketPlan::Unsupported => Err(WebSocketProxyError::TransportNotSupported),
            }
        }
    }
}

async fn relay(
    downstream: &mut DownstreamSocket,
    upstream: &mut UpstreamSocket,
    runtime: Arc<crate::state::EgressRuntimeSnapshot>,
    route: SessionRoute,
    codex_turn_metadata_header_present: bool,
    state: &EgressState,
    shutdown: &mut watch::Receiver<WebSocketShutdown>,
) -> Result<(), WebSocketProxyError> {
    enum Event {
        Downstream(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
        Upstream(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
    }

    let mut request_sequence = 1;
    let mut request_in_flight = true;
    loop {
        let event = tokio::select! {
            biased;
            () = wait_for_session_shutdown(shutdown.clone(), request_in_flight) => {
                close_both(downstream, upstream, CloseCode::Restart, "service restart").await;
                return Ok(());
            }
            event = tokio::time::timeout(
                Duration::from_millis(state.websocket_idle_timeout_ms),
                async {
                    tokio::select! {
                        message = downstream.next() => Event::Downstream(message),
                        message = upstream.next() => Event::Upstream(message),
                    }
                },
            ) => event.map_err(|_| WebSocketProxyError::IdleTimeout)?,
        };

        match event {
            Event::Downstream(Some(Ok(Message::Close(frame)))) => {
                let _ = upstream.send(Message::Close(frame)).await;
                return Ok(());
            }
            Event::Downstream(Some(Ok(message))) => {
                let starts_request = is_response_create_message(&message);
                if starts_request && request_in_flight {
                    return Err(WebSocketProxyError::ConcurrentRequest);
                }
                let message = match prepare_followup_message(
                    message,
                    &route,
                    &mut request_sequence,
                    codex_turn_metadata_header_present,
                    &runtime,
                    state,
                ) {
                    Ok(message) => message,
                    Err(error) => {
                        let _ = upstream
                            .send(close_message(CloseCode::Policy, "route rejected"))
                            .await;
                        return Err(error);
                    }
                };
                send_upstream(
                    upstream,
                    message,
                    state.websocket_idle_timeout_ms,
                    shutdown.clone(),
                )
                .await?;
                if starts_request {
                    request_in_flight = true;
                }
            }
            Event::Upstream(Some(Ok(Message::Close(frame)))) => {
                let _ = downstream.send(Message::Close(frame)).await;
                return Ok(());
            }
            Event::Upstream(Some(Ok(message))) => {
                let terminal = message.to_text().ok().is_some_and(is_terminal_ws_event);
                send_downstream(
                    downstream,
                    message,
                    state.websocket_idle_timeout_ms,
                    shutdown.clone(),
                )
                .await?;
                if terminal {
                    request_in_flight = false;
                    if *shutdown.borrow() != WebSocketShutdown::Running {
                        close_both(downstream, upstream, CloseCode::Restart, "service restart")
                            .await;
                        return Ok(());
                    }
                }
            }
            Event::Downstream(Some(Err(_)) | None) => {
                let _ = upstream
                    .send(close_message(CloseCode::Away, "client closed"))
                    .await;
                return Ok(());
            }
            Event::Upstream(Some(Err(_)) | None) => {
                return Err(WebSocketProxyError::Transport);
            }
        }
    }
}

fn is_response_create_message(message: &Message) -> bool {
    message.to_text().ok().is_some_and(|text| {
        classify_ws_text(text).ok() == Some(WebSocketMessageKind::ResponseCreate)
    })
}

async fn send_upstream(
    upstream: &mut UpstreamSocket,
    message: Message,
    idle_timeout_ms: u64,
    shutdown: watch::Receiver<WebSocketShutdown>,
) -> Result<(), WebSocketProxyError> {
    tokio::select! {
        () = wait_for_force(shutdown) => Err(WebSocketProxyError::Shutdown),
        result = tokio::time::timeout(
            Duration::from_millis(idle_timeout_ms),
            upstream.send(message),
        ) => result
            .map_err(|_| WebSocketProxyError::IdleTimeout)?
            .map_err(|_| WebSocketProxyError::Transport),
    }
}

async fn send_downstream(
    downstream: &mut DownstreamSocket,
    message: Message,
    idle_timeout_ms: u64,
    shutdown: watch::Receiver<WebSocketShutdown>,
) -> Result<(), WebSocketProxyError> {
    tokio::select! {
        () = wait_for_force(shutdown) => Err(WebSocketProxyError::Shutdown),
        result = tokio::time::timeout(
            Duration::from_millis(idle_timeout_ms),
            downstream.send(message),
        ) => result
            .map_err(|_| WebSocketProxyError::IdleTimeout)?
            .map_err(|_| WebSocketProxyError::Transport),
    }
}

fn prepare_followup_message(
    message: Message,
    route: &SessionRoute,
    request_sequence: &mut u64,
    codex_turn_metadata_header_present: bool,
    runtime: &crate::state::EgressRuntimeSnapshot,
    state: &EgressState,
) -> Result<Message, WebSocketProxyError> {
    let Message::Text(text) = message else {
        return Ok(message);
    };
    let kind =
        classify_ws_text(text.as_ref()).map_err(|_| WebSocketProxyError::InvalidFirstMessage)?;
    if kind != WebSocketMessageKind::ResponseCreate {
        return Ok(Message::Text(text));
    }
    let inspected =
        inspect_ws_text(text.as_ref()).map_err(|_| WebSocketProxyError::InvalidFirstMessage)?;
    *request_sequence = request_sequence.saturating_add(1);
    let decision = runtime.resolve(&inspected.model);
    state.observe(EgressEvent::RequestObserved(RequestObserved {
        transport: ObservedTransport::WebSocket,
        path: "/v1/responses".to_owned(),
        sequence: *request_sequence,
        model: inspected.model.clone(),
        route: ObservedRoute::from(&decision),
        previous_response_id_present: inspected.metadata.previous_response_id_present,
        client_metadata_present: inspected.metadata.client_metadata.is_some(),
        codex_turn_metadata_header_present,
    }));
    match (route, decision) {
        (SessionRoute::Official, RouteDecision::BuiltInOfficial) => Ok(Message::Text(text)),
        (
            SessionRoute::ThirdParty(current_provider),
            RouteDecision::ThirdParty {
                provider_id,
                upstream_model_id,
            },
        ) if current_provider == &provider_id => runtime
            .provider(current_provider)
            .ok_or(WebSocketProxyError::ProviderNotAvailable)?
            .profile
            .rewrite_websocket_request(text.as_ref(), upstream_model_id.as_str())
            .map(Message::text)
            .map_err(|_| WebSocketProxyError::InvalidFirstMessage),
        (_, RouteDecision::UnavailableManagedModel) => Err(WebSocketProxyError::ModelNotAvailable),
        _ => Err(WebSocketProxyError::RouteChanged),
    }
}

async fn reject_downstream(downstream: &mut DownstreamSocket, error: &WebSocketProxyError) {
    let code = error.client_code();
    let event = websocket_error_event(code, &error.to_string());
    let _ = downstream.send(Message::text(event)).await;
    let close_code = if matches!(error, WebSocketProxyError::Shutdown) {
        CloseCode::Restart
    } else {
        CloseCode::Policy
    };
    let _ = downstream.send(close_message(close_code, code)).await;
}

async fn close_both(
    downstream: &mut DownstreamSocket,
    upstream: &mut UpstreamSocket,
    code: CloseCode,
    reason: &'static str,
) {
    let _ = downstream.send(close_message(code, reason)).await;
    let _ = upstream.send(close_message(code, reason)).await;
}

pub(crate) fn close_message(code: CloseCode, reason: &'static str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }))
}

fn official_websocket_url(http_base_url: &str) -> Option<String> {
    let websocket_base = http_base_url
        .strip_prefix("https://")
        .map(|remainder| format!("wss://{remainder}"))
        .or_else(|| {
            http_base_url
                .strip_prefix("http://")
                .map(|remainder| format!("ws://{remainder}"))
        })?;
    Some(format!(
        "{}/responses",
        websocket_base.trim_end_matches('/')
    ))
}

fn websocket_config(message_limit: usize) -> WebSocketConfig {
    const WRITE_BUFFER_SIZE: usize = 16 * 1024;
    WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(WRITE_BUFFER_SIZE)
        .max_write_buffer_size(message_limit.saturating_add(WRITE_BUFFER_SIZE))
        .max_message_size(Some(message_limit))
        .max_frame_size(Some(message_limit))
}

pub(crate) async fn wait_for_drain(mut shutdown: watch::Receiver<WebSocketShutdown>) {
    if *shutdown.borrow() != WebSocketShutdown::Running {
        return;
    }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() != WebSocketShutdown::Running {
            return;
        }
    }
}

pub(crate) async fn wait_for_force(mut shutdown: watch::Receiver<WebSocketShutdown>) {
    if *shutdown.borrow() == WebSocketShutdown::Force {
        return;
    }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() == WebSocketShutdown::Force {
            return;
        }
    }
}

async fn wait_for_session_shutdown(
    mut shutdown: watch::Receiver<WebSocketShutdown>,
    request_in_flight: bool,
) {
    loop {
        let state = *shutdown.borrow();
        if state == WebSocketShutdown::Force
            || (!request_in_flight && state == WebSocketShutdown::Draining)
        {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{official_websocket_url, websocket_config};

    #[test]
    fn official_websocket_url_uses_the_same_responses_base() {
        assert_eq!(
            official_websocket_url("https://chatgpt.com/backend-api/codex"),
            Some("wss://chatgpt.com/backend-api/codex/responses".to_owned())
        );
        assert_eq!(
            official_websocket_url("http://127.0.0.1:9000/v1/"),
            Some("ws://127.0.0.1:9000/v1/responses".to_owned())
        );
    }

    #[test]
    fn write_buffer_can_hold_one_maximum_size_message() {
        let message_limit = 32 * 1024 * 1024;
        let config = websocket_config(message_limit);
        assert_eq!(config.max_message_size, Some(message_limit));
        assert!(config.max_write_buffer_size >= message_limit + config.write_buffer_size);
    }
}
