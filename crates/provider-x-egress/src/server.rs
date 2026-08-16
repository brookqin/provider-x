use std::{
    convert::Infallible,
    net::{IpAddr, SocketAddr},
    ops::RangeInclusive,
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited, combinators::BoxBody};
use hyper::{
    Method, Request, Response, StatusCode, Uri, body::Incoming, header, service::service_fn,
};
use hyper_util::rt::TokioIo;
use protocol_openai_responses::{ResponsesPath, http_error_body, inspect_http};
use provider_x_core::RouteDecision;
use provider_x_providers::{HttpResponseAdapter, HttpTarget};
use tokio::{
    net::TcpListener,
    sync::{OwnedSemaphorePermit, watch},
    task::JoinSet,
};
use tokio_util::task::TaskTracker;

use crate::{
    EgressEvent, EgressState, ErrorObserved, FallbackObserved, ObservedRoute, ObservedTransport,
    ProxyError, RequestObserved, UpstreamObserved,
    headers::{
        official_model_catalog_headers, official_request_headers, response_headers,
        rewritten_response_headers, third_party_request_headers,
    },
    request_body::RequestEncoding,
    timeouts::{BoxError, IdleTimeoutBody},
    ws_proxy::{WebSocketShutdown, is_websocket_upgrade, websocket_upgrade},
};

pub(crate) type ProxyBody = BoxBody<Bytes, BoxError>;

pub struct EgressServer {
    listener: TcpListener,
    state: Arc<EgressState>,
}

impl EgressServer {
    /// Binds a loopback listener for the HTTP/SSE ingress.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the listener cannot be bound.
    pub async fn bind(
        address: SocketAddr,
        state: Arc<EgressState>,
    ) -> Result<Self, std::io::Error> {
        validate_loopback(address.ip())?;
        let listener = TcpListener::bind(address).await?;
        Ok(Self { listener, state })
    }

    /// Binds the first available loopback port in an application-managed range.
    ///
    /// Only address-in-use failures advance to the next port. Other I/O failures are returned
    /// immediately so permission or network errors are not hidden as port conflicts.
    ///
    /// # Errors
    ///
    /// Returns an I/O error for a non-loopback address, an empty range, a non-conflict bind
    /// failure, or when every port in the range is already occupied.
    pub async fn bind_first_available(
        ip: IpAddr,
        ports: RangeInclusive<u16>,
        state: Arc<EgressState>,
    ) -> Result<Self, std::io::Error> {
        validate_loopback(ip)?;
        if ports.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "provider-x listener port range must not be empty",
            ));
        }
        let mut last_conflict = None;
        for port in ports {
            match TcpListener::bind(SocketAddr::new(ip, port)).await {
                Ok(listener) => return Ok(Self { listener, state }),
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                    last_conflict = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_conflict.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                "provider-x listener port range is exhausted",
            )
        }))
    }

    /// Returns the effective bound address, including an OS-assigned test port.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the socket address cannot be queried.
    pub fn local_addr(&self) -> Result<SocketAddr, std::io::Error> {
        self.listener.local_addr()
    }

    /// Runs until the shutdown watch becomes true or all senders are dropped.
    ///
    /// # Errors
    ///
    /// Returns an I/O error from the accept loop.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<(), std::io::Error> {
        let Self { listener, state } = self;
        let mut connections = JoinSet::new();
        let websocket_tasks = TaskTracker::new();
        let (websocket_shutdown, _) = watch::channel(WebSocketShutdown::Running);
        loop {
            while connections.try_join_next().is_some() {}
            let permit = tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                    continue;
                }
                permit = Arc::clone(&state.connection_limit).acquire_owned() => {
                    permit.map_err(|_| std::io::Error::other("connection semaphore closed"))?
                }
            };

            let accepted = tokio::select! {
                changed = shutdown.changed() => {
                    drop(permit);
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                    continue;
                }
                accepted = listener.accept() => accepted?,
            };
            let (stream, _) = accepted;
            let state = Arc::clone(&state);
            let websocket_tasks = websocket_tasks.clone();
            let websocket_shutdown = websocket_shutdown.subscribe();
            let connection_shutdown = shutdown.clone();
            connections.spawn(async move {
                let connection_permit = Arc::new(permit);
                let service_permit = Arc::clone(&connection_permit);
                let service = service_fn(move |request| {
                    let state = Arc::clone(&state);
                    let websocket_tasks = websocket_tasks.clone();
                    let websocket_shutdown = websocket_shutdown.clone();
                    let connection_permit = Arc::clone(&service_permit);
                    async move {
                        Ok::<_, Infallible>(
                            handle(
                                request,
                                state,
                                websocket_tasks,
                                websocket_shutdown,
                                connection_permit,
                            )
                            .await,
                        )
                    }
                });
                let connection = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades();
                tokio::pin!(connection);
                let mut connection_shutdown = connection_shutdown;
                tokio::select! {
                    result = &mut connection => {
                        let _ = result;
                    }
                    () = wait_for_shutdown(&mut connection_shutdown) => {
                        connection.as_mut().graceful_shutdown();
                        let _ = connection.await;
                    }
                }
                drop(connection_permit);
            });
        }

        websocket_tasks.close();
        let grace = Duration::from_millis(state.shutdown_grace_ms);
        let _ = websocket_shutdown.send(WebSocketShutdown::Draining);
        let drained = tokio::time::timeout(grace, async {
            while connections.join_next().await.is_some() {}
            websocket_tasks.wait().await;
        })
        .await
        .is_ok();

        if !drained {
            // Active streams received the full grace period. Force-stop only sessions that did
            // not reach a terminal event before the deadline.
            let _ = websocket_shutdown.send(WebSocketShutdown::Force);
            let close_window = Duration::from_secs(2).min(grace);
            let _ = tokio::time::timeout(close_window, async {
                while connections.join_next().await.is_some() {}
                websocket_tasks.wait().await;
            })
            .await;
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        }
        Ok(())
    }
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    let _ = shutdown.changed().await;
}

async fn handle(
    mut request: Request<Incoming>,
    state: Arc<EgressState>,
    websocket_tasks: TaskTracker,
    websocket_shutdown: watch::Receiver<WebSocketShutdown>,
    connection_permit: Arc<OwnedSemaphorePermit>,
) -> Response<ProxyBody> {
    let method = request.method().to_string();
    if let Err(error) = authorize_ingress(&mut request, &state) {
        let path = unauthorized_request_path(request.uri().path());
        observe_proxy_error(
            &state,
            ObservedTransport::Http,
            &method,
            &path,
            false,
            &error,
        );
        return local_error(&error);
    }
    let path = request.uri().path().to_owned();
    if is_websocket_upgrade(&request) {
        if request.headers().contains_key(header::ORIGIN) {
            let error = ProxyError::CrossOriginWebSocket;
            observe_proxy_error(
                &state,
                ObservedTransport::WebSocket,
                &method,
                &path,
                true,
                &error,
            );
            return local_error(&error);
        }
        if state.websocket_fallback_on_upgrade {
            state.observe(EgressEvent::FallbackObserved(FallbackObserved {
                transport: ObservedTransport::WebSocket,
                path: request.uri().path().to_owned(),
                status: StatusCode::UPGRADE_REQUIRED.as_u16(),
            }));
            return websocket_fallback_response();
        }
        return match websocket_upgrade(
            request,
            Arc::clone(&state),
            &websocket_tasks,
            websocket_shutdown,
            connection_permit,
        ) {
            Ok(response) => response,
            Err(error) => {
                observe_proxy_error(
                    &state,
                    ObservedTransport::WebSocket,
                    &method,
                    &path,
                    true,
                    &error,
                );
                local_error(&error)
            }
        };
    }
    match proxy(request, &state).await {
        Ok(response) => {
            if response.status().is_client_error() || response.status().is_server_error() {
                state.observe(EgressEvent::ErrorObserved(ErrorObserved {
                    transport: ObservedTransport::Http,
                    method,
                    path,
                    ingress_authorized: true,
                    status: Some(response.status().as_u16()),
                    code: "upstream_http_status".to_owned(),
                    message: "upstream returned an HTTP error status".to_owned(),
                }));
            }
            response
        }
        Err(error) => {
            observe_proxy_error(
                &state,
                ObservedTransport::Http,
                &method,
                &path,
                true,
                &error,
            );
            local_error(&error)
        }
    }
}

fn observe_proxy_error(
    state: &EgressState,
    transport: ObservedTransport,
    method: &str,
    path: &str,
    ingress_authorized: bool,
    error: &ProxyError,
) {
    state.observe(EgressEvent::ErrorObserved(ErrorObserved {
        transport,
        method: method.to_owned(),
        path: path.to_owned(),
        ingress_authorized,
        status: Some(proxy_error_status(error).as_u16()),
        code: error.code().to_owned(),
        message: error.to_string(),
    }));
}

fn unauthorized_request_path(path: &str) -> String {
    let Some(candidate_and_suffix) = path.strip_prefix('/') else {
        return path.to_owned();
    };
    let (candidate, suffix) = candidate_and_suffix
        .split_once('/')
        .map_or((candidate_and_suffix, None), |(candidate, suffix)| {
            (candidate, Some(suffix))
        });
    if !looks_like_capability(candidate) {
        return path.to_owned();
    }

    suffix.map_or_else(
        || "/<redacted-capability>".to_owned(),
        |suffix| format!("/<redacted-capability>/{suffix}"),
    )
}

fn looks_like_capability(segment: &str) -> bool {
    segment.len() == 64
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_loopback(ip: IpAddr) -> Result<(), std::io::Error> {
    if ip.is_loopback() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "provider-x listener must be loopback",
        ))
    }
}

fn authorize_ingress(
    request: &mut Request<Incoming>,
    state: &EgressState,
) -> Result<(), ProxyError> {
    let authorized_path = state
        .authorized_path(request.uri().path())
        .ok_or(ProxyError::IngressNotFound)?;
    let rewritten = request.uri().query().map_or_else(
        || authorized_path.to_owned(),
        |query| format!("{authorized_path}?{query}"),
    );
    *request.uri_mut() = rewritten
        .parse::<Uri>()
        .map_err(|_| ProxyError::InvalidRequest("failed to normalize ingress path".to_owned()))?;
    Ok(())
}

fn websocket_fallback_response() -> Response<ProxyBody> {
    let mut response = Response::new(
        Full::new(Bytes::new())
            .map_err(|never| match never {})
            .boxed(),
    );
    *response.status_mut() = StatusCode::UPGRADE_REQUIRED;
    response.headers_mut().insert(
        header::UPGRADE,
        header::HeaderValue::from_static("websocket"),
    );
    response
}

#[allow(clippy::too_many_lines)] // Keeps one request's route, conversion, and stream handoff explicit.
async fn proxy(
    request: Request<Incoming>,
    state: &EgressState,
) -> Result<Response<ProxyBody>, ProxyError> {
    if request.method() == Method::GET && request.uri().path() == "/v1/models" {
        return proxy_official_models(request, state).await;
    }
    let (parts, incoming) = request.into_parts();
    let original_body = collect_request_body(incoming, state).await?;
    // Only protocol-owned inference requests use model routing. Every other Codex API call stays
    // on the official control plane even if its payload happens to contain a `model` field.
    if parts.method != Method::POST || ResponsesPath::try_from(parts.uri.path()).is_err() {
        return proxy_official_request(parts, original_body, state).await;
    }
    let encoding = RequestEncoding::from_headers(&parts.headers)?;
    let decoded_body = encoding
        .decode(original_body.clone(), state.request_body_limit_bytes)
        .await?;
    let inspected = inspect_http(parts.uri.path(), &decoded_body)
        .map_err(|error| ProxyError::InvalidRequest(error.to_string()))?;
    let runtime = state.runtime.load_full();
    let decision = runtime.resolve(&inspected.model);
    let observed_route = ObservedRoute::from(&decision);
    state.observe(EgressEvent::RequestObserved(RequestObserved {
        transport: ObservedTransport::Http,
        path: parts.uri.path().to_owned(),
        sequence: 1,
        model: inspected.model.clone(),
        route: observed_route.clone(),
        previous_response_id_present: inspected.metadata.previous_response_id_present,
        client_metadata_present: inspected.metadata.client_metadata.is_some(),
        codex_turn_metadata_header_present: parts.headers.contains_key("x-codex-turn-metadata"),
    }));

    let (target, response_adapter, headers, body, client) = match decision {
        RouteDecision::BuiltInOfficial => (
            HttpTarget::PreserveIngressPath(state.official_base_url.to_string()),
            HttpResponseAdapter::Passthrough,
            official_request_headers(&parts.headers),
            original_body,
            &state.official_client,
        ),
        RouteDecision::UnavailableManagedModel => return Err(ProxyError::ModelNotAvailable),
        RouteDecision::ThirdParty {
            provider_id,
            upstream_model_id,
        } => {
            let provider = runtime
                .provider(&provider_id)
                .ok_or(ProxyError::ProviderNotAvailable)?;
            let prepared = provider
                .profile
                .prepare_http_request(
                    &decoded_body,
                    upstream_model_id.as_str(),
                    state.request_body_limit_bytes,
                )
                .map_err(|error| ProxyError::InvalidRequest(error.to_string()))?;
            (
                prepared.target,
                prepared.response_adapter,
                third_party_request_headers(
                    &parts.headers,
                    &provider.config.auth,
                    &provider.profile,
                )?,
                prepared.body,
                &provider.client,
            )
        }
    };

    let uri = match target {
        HttpTarget::PreserveIngressPath(base_url) => upstream_uri(&base_url, &parts.uri)?,
        HttpTarget::Exact(url) => url.parse().map_err(|_| ProxyError::InvalidUpstreamUri)?,
    };
    let mut upstream = Request::builder()
        .method(parts.method)
        .uri(uri)
        .body(Full::new(body))
        .map_err(|_| ProxyError::RequestBuild)?;
    *upstream.headers_mut() = headers;

    let response = tokio::time::timeout(
        Duration::from_millis(state.response_headers_timeout_ms),
        client.request(upstream),
    )
    .await
    .map_err(|_| ProxyError::ResponseHeadersTimeout)?
    .map_err(|error| classify_upstream_error(&error))?;
    state.observe(EgressEvent::UpstreamObserved(UpstreamObserved {
        transport: ObservedTransport::Http,
        route: observed_route,
        status: response.status().as_u16(),
    }));
    let (parts, body) = response.into_parts();
    let stream_timeout = Duration::from_millis(state.stream_idle_timeout_ms);
    let rewrites_success_body =
        !matches!(&response_adapter, HttpResponseAdapter::Passthrough) && parts.status.is_success();
    let downstream_body = match response_adapter {
        HttpResponseAdapter::OpenaiChatCompletions(tool_names) if parts.status.is_success() => {
            let decoder = protocol_openai_chat_completions::ChatSseDecoder::with_tool_names(
                state.request_body_limit_bytes,
                tool_names,
            );
            crate::chat_http_bridge::ChatCompletionBody::new(body, decoder, stream_timeout).boxed()
        }
        HttpResponseAdapter::AnthropicMessages(tool_names) if parts.status.is_success() => {
            let decoder = protocol_anthropic_messages::AnthropicSseDecoder::with_tool_names(
                state.request_body_limit_bytes,
                tool_names,
            );
            crate::anthropic_http_bridge::AnthropicMessageBody::new(body, decoder, stream_timeout)
                .boxed()
        }
        HttpResponseAdapter::Passthrough
        | HttpResponseAdapter::OpenaiChatCompletions(_)
        | HttpResponseAdapter::AnthropicMessages(_) => {
            IdleTimeoutBody::new(body, stream_timeout).boxed()
        }
    };
    let mut downstream = Response::new(downstream_body);
    *downstream.status_mut() = parts.status;
    *downstream.version_mut() = parts.version;
    *downstream.headers_mut() = if rewrites_success_body {
        rewritten_response_headers(&parts.headers)
    } else {
        response_headers(&parts.headers)
    };
    Ok(downstream)
}

async fn proxy_official_request(
    parts: hyper::http::request::Parts,
    body: Bytes,
    state: &EgressState,
) -> Result<Response<ProxyBody>, ProxyError> {
    let uri = upstream_uri(&state.official_base_url, &parts.uri)?;
    let mut upstream = Request::builder()
        .method(parts.method)
        .uri(uri)
        .body(Full::new(body))
        .map_err(|_| ProxyError::RequestBuild)?;
    *upstream.headers_mut() = official_request_headers(&parts.headers);

    let response = tokio::time::timeout(
        Duration::from_millis(state.response_headers_timeout_ms),
        state.official_client.request(upstream),
    )
    .await
    .map_err(|_| ProxyError::ResponseHeadersTimeout)?
    .map_err(|error| classify_upstream_error(&error))?;
    let (parts, body) = response.into_parts();
    let mut downstream = Response::new(
        IdleTimeoutBody::new(body, Duration::from_millis(state.stream_idle_timeout_ms)).boxed(),
    );
    *downstream.status_mut() = parts.status;
    *downstream.version_mut() = parts.version;
    *downstream.headers_mut() = response_headers(&parts.headers);
    Ok(downstream)
}

async fn collect_request_body(
    incoming: Incoming,
    state: &EgressState,
) -> Result<Bytes, ProxyError> {
    tokio::time::timeout(
        Duration::from_millis(state.request_body_timeout_ms),
        Limited::new(incoming, state.request_body_limit_bytes).collect(),
    )
    .await
    .map_err(|_| ProxyError::RequestBodyTimeout)?
    .map_err(|error| {
        if error.downcast_ref::<LengthLimitError>().is_some() {
            ProxyError::BodyTooLarge
        } else {
            ProxyError::InvalidRequest("failed to read request body".to_owned())
        }
    })
    .map(http_body_util::Collected::to_bytes)
}

async fn proxy_official_models(
    request: Request<Incoming>,
    state: &EgressState,
) -> Result<Response<ProxyBody>, ProxyError> {
    let (parts, _body) = request.into_parts();
    let runtime = state.runtime.load_full();
    let uri = upstream_uri(&state.official_base_url, &parts.uri)?;
    let mut upstream = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Full::new(Bytes::new()))
        .map_err(|_| ProxyError::RequestBuild)?;
    *upstream.headers_mut() = if runtime.catalog_overlay.is_empty() {
        official_request_headers(&parts.headers)
    } else {
        official_model_catalog_headers(&parts.headers)
    };
    let response = tokio::time::timeout(
        Duration::from_millis(state.response_headers_timeout_ms),
        state.official_client.request(upstream),
    )
    .await
    .map_err(|_| ProxyError::ResponseHeadersTimeout)?
    .map_err(|error| classify_upstream_error(&error))?;
    let (parts, body) = response.into_parts();
    if !parts.status.is_success() || runtime.catalog_overlay.is_empty() {
        let mut downstream = Response::new(
            IdleTimeoutBody::new(body, Duration::from_millis(state.stream_idle_timeout_ms)).boxed(),
        );
        *downstream.status_mut() = parts.status;
        *downstream.version_mut() = parts.version;
        *downstream.headers_mut() = response_headers(&parts.headers);
        return Ok(downstream);
    }

    let collected = tokio::time::timeout(
        Duration::from_millis(state.stream_idle_timeout_ms),
        Limited::new(body, state.request_body_limit_bytes).collect(),
    )
    .await
    .map_err(|_| ProxyError::ModelCatalogBodyTimeout)?
    .map_err(|error| {
        if error.downcast_ref::<LengthLimitError>().is_some() {
            ProxyError::ModelCatalogBodyTooLarge
        } else {
            ProxyError::InvalidOfficialModelCatalog
        }
    })?;
    let merged = runtime
        .catalog_overlay
        .merge(&collected.to_bytes())
        .map_err(|_| ProxyError::InvalidOfficialModelCatalog)?;
    let body = Bytes::from(merged.bytes);
    let mut downstream = Response::new(
        Full::new(body.clone())
            .map_err(|never| match never {})
            .boxed(),
    );
    *downstream.status_mut() = parts.status;
    *downstream.version_mut() = parts.version;
    *downstream.headers_mut() = rewritten_response_headers(&parts.headers);
    downstream.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    downstream.headers_mut().insert(
        header::CONTENT_LENGTH,
        header::HeaderValue::from_str(&body.len().to_string())
            .map_err(|_| ProxyError::RequestBuild)?,
    );
    Ok(downstream)
}

fn upstream_uri(base_url: &str, incoming: &Uri) -> Result<Uri, ProxyError> {
    let path_and_query = incoming
        .path_and_query()
        .map_or(incoming.path(), hyper::http::uri::PathAndQuery::as_str);
    let suffix = path_and_query
        .strip_prefix("/v1")
        .ok_or(ProxyError::InvalidUpstreamUri)?;
    format!("{}{suffix}", base_url.trim_end_matches('/'))
        .parse()
        .map_err(|_| ProxyError::InvalidUpstreamUri)
}

fn classify_upstream_error(error: &hyper_util::client::legacy::Error) -> ProxyError {
    if error.is_connect() {
        ProxyError::UpstreamConnect
    } else {
        ProxyError::Upstream
    }
}

fn local_error(error: &ProxyError) -> Response<ProxyBody> {
    let status = proxy_error_status(error);
    let mut response = Response::new(
        Full::new(http_error_body(&error.to_string()))
            .map_err(|never| match never {})
            .boxed(),
    );
    *response.status_mut() = status;
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    response
}

fn proxy_error_status(error: &ProxyError) -> StatusCode {
    match error {
        ProxyError::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        ProxyError::UnsupportedContentEncoding => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ProxyError::RequestBodyTimeout => StatusCode::REQUEST_TIMEOUT,
        ProxyError::InvalidRequest(_) | ProxyError::InvalidWebSocketHandshake => {
            StatusCode::BAD_REQUEST
        }
        ProxyError::IngressNotFound | ProxyError::ModelNotAvailable => StatusCode::NOT_FOUND,
        ProxyError::CrossOriginWebSocket => StatusCode::FORBIDDEN,
        ProxyError::ResponseHeadersTimeout | ProxyError::ModelCatalogBodyTimeout => {
            StatusCode::GATEWAY_TIMEOUT
        }
        ProxyError::ModelCatalogBodyTooLarge | ProxyError::InvalidOfficialModelCatalog => {
            StatusCode::BAD_GATEWAY
        }
        ProxyError::ProviderNotAvailable
        | ProxyError::InvalidUpstreamUri
        | ProxyError::RequestBuild
        | ProxyError::UpstreamConnect
        | ProxyError::Upstream => StatusCode::BAD_GATEWAY,
    }
}
