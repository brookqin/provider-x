use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, body::Incoming, header};
use provider_x_core::RouteDecision;
use provider_x_protocol::{
    BridgeFailure, WsHttpAction, WsHttpEventDecoder, WsHttpProtocolAdapter, WsHttpStreamOutcome,
};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

use crate::{
    EgressEvent, EgressState, ObservedRoute, ObservedTransport, ObservedWebSocketReason,
    ObservedWebSocketStage, UpstreamObserved,
    headers::third_party_request_headers,
    state::ProviderEgress,
    ws_proxy::{
        DownstreamSocket, WebSocketFailure, WebSocketProxyError, WebSocketShutdown, wait_for_drain,
        wait_for_force,
    },
};

pub(crate) struct WsHttpSessionContext<'a> {
    pub provider: &'a ProviderEgress,
    pub runtime: &'a crate::state::EgressRuntimeSnapshot,
    pub request_headers: &'a hyper::HeaderMap,
    pub first_text: &'a str,
    pub upstream_model: String,
    pub observed_route: ObservedRoute,
    pub codex_turn_metadata_header_present: bool,
    pub session_id: u64,
    pub state: &'a EgressState,
}

pub(crate) async fn run<A: WsHttpProtocolAdapter>(
    downstream: &mut DownstreamSocket,
    context: WsHttpSessionContext<'_>,
    shutdown: &mut watch::Receiver<WebSocketShutdown>,
) -> Result<(), WebSocketProxyError> {
    let adapter = A::new_session(
        context.upstream_model.clone(),
        context.state.request_body_limit_bytes,
    );
    run_with_adapter(downstream, context, adapter, shutdown).await
}

pub(crate) async fn run_with_adapter<A: WsHttpProtocolAdapter>(
    downstream: &mut DownstreamSocket,
    context: WsHttpSessionContext<'_>,
    mut adapter: A,
    shutdown: &mut watch::Receiver<WebSocketShutdown>,
) -> Result<(), WebSocketProxyError> {
    let WsHttpSessionContext {
        provider,
        runtime,
        request_headers,
        first_text,
        upstream_model: _,
        observed_route,
        codex_turn_metadata_header_present,
        session_id,
        state,
    } = context;
    let mut next_text = Some(first_text.to_owned());
    let mut request_sequence = 1_u64;

    loop {
        let text = if let Some(text) = next_text.take() {
            text
        } else {
            let text = receive_next_create(downstream, state, shutdown).await?;
            request_sequence = request_sequence.saturating_add(1);
            validate_followup_route(
                &text,
                provider,
                runtime,
                request_sequence,
                codex_turn_metadata_header_present,
                session_id,
                state,
            )?;
            text
        };
        match adapter.prepare_action(&text).map_err(map_bridge_failure)? {
            WsHttpAction::Warmup { events } => {
                send_events(downstream, events, state, shutdown.clone()).await?;
            }
            WsHttpAction::Request { body, pending } => {
                let outcome = request_http::<A>(
                    &adapter,
                    downstream,
                    provider,
                    request_headers,
                    body,
                    observed_route.clone(),
                    session_id,
                    state,
                    shutdown,
                )
                .await?;
                if !outcome.terminal {
                    return Err(WebSocketProxyError::InvalidUpstreamStream);
                }
                if outcome.completed {
                    adapter
                        .commit_outcome(pending, outcome.commit)
                        .map_err(map_bridge_failure)?;
                }
                if *shutdown.borrow() != WebSocketShutdown::Running {
                    send_service_restart(downstream).await;
                    return Ok(());
                }
            }
        }
    }
}

fn validate_followup_route(
    text: &str,
    provider: &ProviderEgress,
    runtime: &crate::state::EgressRuntimeSnapshot,
    sequence: u64,
    codex_turn_metadata_header_present: bool,
    session_id: u64,
    state: &EgressState,
) -> Result<(), WebSocketProxyError> {
    let inspected = protocol_openai_responses::inspect_ws_text(text)
        .map_err(|_| WebSocketProxyError::InvalidFirstMessage)?;
    let decision = runtime.resolve(&inspected.model);
    state.observe(EgressEvent::RequestObserved(crate::RequestObserved {
        transport: ObservedTransport::WebSocket,
        path: "/v1/responses".to_owned(),
        session_id: Some(session_id),
        sequence,
        model: inspected.model,
        route: ObservedRoute::from(&decision),
        previous_response_id_present: inspected.metadata.previous_response_id_present,
        client_metadata_present: inspected.metadata.client_metadata.is_some(),
        codex_turn_metadata_header_present,
    }));
    match decision {
        RouteDecision::ThirdParty { provider_id, .. } if provider_id == provider.config.id => {
            Ok(())
        }
        RouteDecision::UnavailableManagedModel => Err(WebSocketProxyError::ModelNotAvailable),
        RouteDecision::BuiltInOfficial | RouteDecision::ThirdParty { .. } => {
            Err(WebSocketProxyError::RouteChanged)
        }
    }
}

fn map_bridge_failure(error: BridgeFailure) -> WebSocketProxyError {
    match error {
        BridgeFailure::SessionHistoryLimit => WebSocketProxyError::SessionHistoryLimit,
        BridgeFailure::InvalidStream => WebSocketProxyError::InvalidUpstreamStream,
        BridgeFailure::InvalidRequest => WebSocketProxyError::InvalidFirstMessage,
    }
}

async fn receive_next_create(
    downstream: &mut DownstreamSocket,
    state: &EgressState,
    shutdown: &mut watch::Receiver<WebSocketShutdown>,
) -> Result<String, WebSocketProxyError> {
    loop {
        let message = tokio::select! {
            biased;
            () = wait_for_drain(shutdown.clone()) => {
                send_service_restart(downstream).await;
                return Err(WebSocketProxyError::ClientClosed);
            }
            message = tokio::time::timeout(
                Duration::from_millis(state.websocket_idle_timeout_ms),
                downstream.next(),
            ) => message
                .map_err(|_| WebSocketProxyError::IdleTimeout(WebSocketFailure::downstream(
                    ObservedWebSocketStage::Relay,
                    ObservedWebSocketReason::Timeout,
                )))?
                .ok_or(WebSocketProxyError::ClientClosed)?
                .map_err(|_| WebSocketProxyError::Transport(WebSocketFailure::downstream(
                    ObservedWebSocketStage::Relay,
                    ObservedWebSocketReason::Transport,
                )))?,
        };
        match message {
            Message::Text(text) => return Ok(text.to_string()),
            Message::Ping(payload) => {
                send_downstream(downstream, Message::Pong(payload), state, shutdown.clone())
                    .await?;
            }
            Message::Pong(_) => {}
            Message::Close(_) => return Err(WebSocketProxyError::ClientClosed),
            Message::Binary(_) | Message::Frame(_) => {
                return Err(WebSocketProxyError::InvalidFirstMessage);
            }
        }
    }
}

async fn send_service_restart(downstream: &mut DownstreamSocket) {
    let _ = downstream
        .send(crate::ws_proxy::close_message(
            tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Restart,
            "service restart",
        ))
        .await;
}

#[allow(clippy::too_many_arguments)] // Keeps protocol adapter state beside the existing request context.
async fn request_http<A: WsHttpProtocolAdapter>(
    adapter: &A,
    downstream: &mut DownstreamSocket,
    provider: &ProviderEgress,
    source_headers: &hyper::HeaderMap,
    body: Bytes,
    observed_route: ObservedRoute,
    session_id: u64,
    state: &EgressState,
    shutdown: &mut watch::Receiver<WebSocketShutdown>,
) -> Result<WsHttpStreamOutcome<A::Commit>, WebSocketProxyError> {
    let uri: hyper::Uri = provider
        .profile
        .websocket_http_url()
        .parse()
        .map_err(|_| WebSocketProxyError::ProviderNotAvailable)?;
    let mut headers =
        third_party_request_headers(source_headers, &provider.config.auth, &provider.profile)
            .map_err(|_| WebSocketProxyError::ProviderNotAvailable)?;
    let websocket_headers: Vec<_> = headers
        .keys()
        .filter(|name| name.as_str().starts_with("sec-websocket-"))
        .cloned()
        .collect();
    for name in websocket_headers {
        headers.remove(name);
    }
    headers.remove(header::ACCEPT_ENCODING);
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        header::ACCEPT,
        header::HeaderValue::from_static("text/event-stream"),
    );
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .body(Full::new(body))
        .map_err(|_| {
            WebSocketProxyError::UpstreamConnect(WebSocketFailure::upstream(
                ObservedWebSocketStage::RequestBuild,
                ObservedWebSocketReason::InvalidEndpoint,
            ))
        })?;
    *request.headers_mut() = headers;

    let response = tokio::select! {
        () = wait_for_force(shutdown.clone()) => return Err(WebSocketProxyError::Shutdown),
        result = tokio::time::timeout(
            Duration::from_millis(state.response_headers_timeout_ms),
            provider.client.request(request),
        ) => result
            .map_err(|_| WebSocketProxyError::UpstreamConnect(WebSocketFailure::upstream(
                ObservedWebSocketStage::Connect,
                ObservedWebSocketReason::Timeout,
            )))?
            .map_err(|_| WebSocketProxyError::UpstreamConnect(WebSocketFailure::upstream(
                ObservedWebSocketStage::Connect,
                ObservedWebSocketReason::Transport,
            )))?,
    };
    state.observe(EgressEvent::UpstreamObserved(UpstreamObserved {
        transport: ObservedTransport::Http,
        session_id: Some(session_id),
        route: observed_route,
        status: response.status().as_u16(),
    }));
    if !response.status().is_success() {
        return Err(WebSocketProxyError::UpstreamStatus(response.status()));
    }
    collect_sse::<A>(adapter, downstream, response.into_body(), state, shutdown).await
}

async fn collect_sse<A: WsHttpProtocolAdapter>(
    adapter: &A,
    downstream: &mut DownstreamSocket,
    mut body: Incoming,
    state: &EgressState,
    shutdown: &mut watch::Receiver<WebSocketShutdown>,
) -> Result<WsHttpStreamOutcome<A::Commit>, WebSocketProxyError> {
    let mut decoder = adapter.new_decoder(state.request_body_limit_bytes);
    loop {
        enum Event {
            Upstream(Option<Result<hyper::body::Frame<Bytes>, hyper::Error>>),
            Downstream(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
        }
        let event = tokio::select! {
            () = wait_for_force(shutdown.clone()) => return Err(WebSocketProxyError::Shutdown),
            result = tokio::time::timeout(
                Duration::from_millis(state.stream_idle_timeout_ms),
                async {
                    tokio::select! {
                        frame = body.frame() => Event::Upstream(frame),
                        message = downstream.next() => Event::Downstream(message),
                    }
                },
            ) => result.map_err(|_| WebSocketProxyError::IdleTimeout(WebSocketFailure::undirected(
                ObservedWebSocketStage::ResponseStream,
                ObservedWebSocketReason::Timeout,
            )))?,
        };
        let frame = match event {
            Event::Upstream(frame) => frame.transpose().map_err(|_| {
                WebSocketProxyError::Transport(WebSocketFailure::upstream(
                    ObservedWebSocketStage::ResponseStream,
                    ObservedWebSocketReason::Transport,
                ))
            })?,
            Event::Downstream(Some(Ok(Message::Ping(payload)))) => {
                send_downstream(downstream, Message::Pong(payload), state, shutdown.clone())
                    .await?;
                continue;
            }
            Event::Downstream(Some(Ok(Message::Pong(_)))) => continue,
            Event::Downstream(Some(Ok(Message::Close(_))) | None) => {
                return Err(WebSocketProxyError::ClientClosed);
            }
            Event::Downstream(Some(Ok(Message::Text(_)))) => {
                return Err(WebSocketProxyError::ConcurrentRequest);
            }
            Event::Downstream(Some(Ok(Message::Binary(_) | Message::Frame(_)) | Err(_))) => {
                return Err(WebSocketProxyError::Transport(
                    WebSocketFailure::downstream(
                        ObservedWebSocketStage::ResponseStream,
                        ObservedWebSocketReason::Transport,
                    ),
                ));
            }
        };
        let Some(frame) = frame else { break };
        if let Ok(data) = frame.into_data() {
            let events = decoder.push(&data).map_err(map_bridge_failure)?;
            send_events(downstream, events, state, shutdown.clone()).await?;
            if decoder.is_terminal() {
                break;
            }
        }
    }
    let events = decoder.finish().map_err(map_bridge_failure)?;
    send_events(downstream, events, state, shutdown.clone()).await?;
    Ok(decoder.into_outcome())
}

async fn send_events(
    downstream: &mut DownstreamSocket,
    events: Vec<String>,
    state: &EgressState,
    shutdown: watch::Receiver<WebSocketShutdown>,
) -> Result<(), WebSocketProxyError> {
    for event in events {
        send_downstream(downstream, Message::text(event), state, shutdown.clone()).await?;
    }
    Ok(())
}

async fn send_downstream(
    downstream: &mut DownstreamSocket,
    message: Message,
    state: &EgressState,
    shutdown: watch::Receiver<WebSocketShutdown>,
) -> Result<(), WebSocketProxyError> {
    tokio::select! {
        () = wait_for_force(shutdown) => Err(WebSocketProxyError::Shutdown),
        result = tokio::time::timeout(
            Duration::from_millis(state.websocket_idle_timeout_ms),
            downstream.send(message),
        ) => result
            .map_err(|_| WebSocketProxyError::IdleTimeout(WebSocketFailure::downstream(
                ObservedWebSocketStage::ResponseStream,
                ObservedWebSocketReason::Timeout,
            )))?
            .map_err(|_| WebSocketProxyError::Transport(WebSocketFailure::downstream(
                ObservedWebSocketStage::ResponseStream,
                ObservedWebSocketReason::Transport,
            ))),
    }
}
