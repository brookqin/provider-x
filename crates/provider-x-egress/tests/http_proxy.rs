use std::{collections::BTreeMap, convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt, stream};
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::{Request, Response, body::Frame, body::Incoming, service::service_fn};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioIo},
};
use protocol_openai_responses::is_terminal_ws_event;
use provider_x_core::{
    AuthConfig, CatalogModelId, CodexConfig, EndpointConfig, ListenerConfig, ModelCacheDocument,
    ModelId, ModelPublicationStatus, ProtocolId, ProviderConfig, ProviderId, ProviderModelCache,
    ProviderModelSource, ProviderModelSpec, ProvidersDocument, TimeoutConfig, TransportConfig,
};
use provider_x_egress::{
    EgressEvent, EgressObserver, EgressServer, EgressState, IngressCapability, ObservedRoute,
    ObservedTransport,
};
use serde_json::Value;
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot, watch},
};
use tokio_tungstenite::{
    accept_async, accept_hdr_async, connect_async,
    tungstenite::{
        client::IntoClientRequest,
        handshake::server::{Request as WsHandshakeRequest, Response as WsHandshakeResponse},
        protocol::{Message, frame::coding::CloseCode},
    },
};

const TEST_CAPABILITY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn ingress_capability() -> IngressCapability {
    IngressCapability::from_hex(TEST_CAPABILITY).unwrap()
}

fn ingress_http_url(address: SocketAddr, path_and_query: &str) -> String {
    format!("http://{address}/{TEST_CAPABILITY}{path_and_query}")
}

fn ingress_websocket_url(address: SocketAddr) -> String {
    format!("ws://{address}/{TEST_CAPABILITY}/v1/responses")
}

#[derive(Debug)]
struct CapturedRequest {
    method: String,
    path_and_query: String,
    authorization: Option<String>,
    x_api_key: Option<String>,
    anthropic_version: Option<String>,
    chatgpt_account_id: Option<String>,
    content_encoding: Option<String>,
    body: Bytes,
}

#[derive(Default)]
struct CollectingObserver(std::sync::Mutex<Vec<EgressEvent>>);

impl EgressObserver for CollectingObserver {
    fn record(&self, event: EgressEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl CollectingObserver {
    fn records(&self) -> Vec<EgressEvent> {
        self.0.lock().unwrap().clone()
    }
}

fn providers(provider: Option<ProviderConfig>, body_limit: u64) -> ProvidersDocument {
    ProvidersDocument {
        schema_version: 1,
        listener: ListenerConfig {
            host: "127.0.0.1".to_owned(),
            port: 43119,
            request_body_limit_bytes: body_limit,
            max_connections: 128,
        },
        timeouts: TimeoutConfig {
            request_body_ms: 30_000,
            connect_ms: 10_000,
            response_headers_ms: 5_000,
            stream_idle_ms: 300_000,
            websocket_idle_ms: 300_000,
            shutdown_grace_ms: 30_000,
        },
        codex: CodexConfig {
            manage_user_config: true,
        },
        providers: provider.into_iter().collect(),
    }
}

fn provider(upstream: SocketAddr) -> ProviderConfig {
    ProviderConfig {
        id: ProviderId::new("provider-a").unwrap(),
        name: "Provider A".to_owned(),
        description: None,
        enabled: true,
        protocol: ProtocolId::OpenaiResponses,
        anthropic_thinking: None,
        endpoints: EndpointConfig {
            http: format!("http://{upstream}/v1"),
            websocket: None,
            models: None,
        },
        auth: AuthConfig::Bearer {
            api_key: "third-party-secret".to_owned(),
        },
        transports: TransportConfig {
            http_sse: true,
            websocket: false,
        },
    }
}

fn chat_provider(upstream: SocketAddr) -> ProviderConfig {
    let mut provider = provider(upstream);
    provider.protocol = ProtocolId::OpenaiChatCompletions;
    provider
}

fn anthropic_provider(upstream: SocketAddr) -> ProviderConfig {
    let mut provider = provider(upstream);
    provider.protocol = ProtocolId::AnthropicMessages;
    provider.endpoints.http = format!("http://{upstream}");
    provider
}

fn websocket_provider(upstream: SocketAddr) -> ProviderConfig {
    let mut provider = provider(upstream);
    provider.endpoints.websocket = Some(format!("ws://{upstream}/v1/responses"));
    provider.transports.websocket = true;
    provider
}

#[derive(Debug)]
struct CapturedWebSocketMessage {
    path: String,
    authorization: Option<String>,
    chatgpt_account_id: Option<String>,
    text: String,
}

#[allow(clippy::result_large_err)] // Required by tungstenite's handshake callback signature.
async fn spawn_websocket_upstream() -> (
    SocketAddr,
    mpsc::Receiver<CapturedWebSocketMessage>,
    oneshot::Receiver<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (captured_tx, captured_rx) = mpsc::channel(8);
    let (closed_tx, closed_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let handshake = Arc::new(std::sync::Mutex::new(None));
        let handshake_for_callback = Arc::clone(&handshake);
        let mut websocket = accept_hdr_async(
            stream,
            move |request: &WsHandshakeRequest, response: WsHandshakeResponse| {
                let authorization = request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned);
                let chatgpt_account_id = request
                    .headers()
                    .get("chatgpt-account-id")
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned);
                *handshake_for_callback.lock().unwrap() = Some((
                    request.uri().path().to_owned(),
                    authorization,
                    chatgpt_account_id,
                ));
                Ok(response)
            },
        )
        .await
        .unwrap();
        let (path, authorization, chatgpt_account_id) = handshake.lock().unwrap().take().unwrap();
        while let Some(message) = websocket.next().await {
            let Ok(message) = message else {
                break;
            };
            match message {
                Message::Text(text) => {
                    captured_tx
                        .send(CapturedWebSocketMessage {
                            path: path.clone(),
                            authorization: authorization.clone(),
                            chatgpt_account_id: chatgpt_account_id.clone(),
                            text: text.to_string(),
                        })
                        .await
                        .unwrap();
                    websocket
                        .send(Message::text(
                            serde_json::json!({
                                "type": "response.completed",
                                "response": {"id": "resp_test"}
                            })
                            .to_string(),
                        ))
                        .await
                        .unwrap();
                }
                Message::Close(_) => break,
                Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
        let _ = closed_tx.send(());
    });
    (address, captured_rx, closed_rx)
}

async fn spawn_delayed_websocket_upstream(delay: Duration) -> (SocketAddr, oneshot::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (closed_tx, closed_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut websocket = accept_async(stream).await.unwrap();
        while let Some(message) = websocket.next().await {
            match message.unwrap() {
                Message::Text(_) => {
                    websocket
                        .send(Message::text(
                            serde_json::json!({
                                "type": "response.output_text.delta",
                                "delta": "working"
                            })
                            .to_string(),
                        ))
                        .await
                        .unwrap();
                    tokio::time::sleep(delay).await;
                    websocket
                        .send(Message::text(
                            serde_json::json!({
                                "type": "response.completed",
                                "response": {"id": "resp_delayed"}
                            })
                            .to_string(),
                        ))
                        .await
                        .unwrap();
                }
                Message::Close(_) => break,
                Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
        let _ = closed_tx.send(());
    });
    (address, closed_rx)
}

fn catalog_cache(providers: &ProvidersDocument) -> ModelCacheDocument {
    let upstream_model_id = ModelId::new("coder").unwrap();
    ModelCacheDocument {
        schema_version: 1,
        providers: providers
            .providers
            .iter()
            .map(|provider| {
                (
                    provider.id.clone(),
                    ProviderModelCache {
                        config_fingerprint: provider.routing_fingerprint().unwrap(),
                        last_successful_refresh_at: "2026-08-12T00:00:00Z".to_owned(),
                        source: ProviderModelSource {
                            protocol: provider.protocol,
                            endpoint: format!("{}/models", provider.endpoints.http),
                        },
                        models: vec![ProviderModelSpec {
                            catalog_model_id: CatalogModelId::for_provider(
                                &provider.id,
                                &upstream_model_id,
                            ),
                            upstream_model_id: upstream_model_id.clone(),
                            display_name: "Coder".to_owned(),
                            publication_status: ModelPublicationStatus::Ready,
                            context_window: Some(128_000),
                            supported_reasoning_levels: vec!["low".to_owned()],
                            supports_parallel_tool_calls: Some(true),
                            supports_search_tool: Some(false),
                            metadata_sources: BTreeMap::new(),
                        }],
                    },
                )
            })
            .collect(),
    }
}

async fn spawn_upstream(
    chunks: Vec<&'static str>,
) -> (SocketAddr, mpsc::Receiver<CapturedRequest>) {
    let chunks: Vec<Bytes> = chunks
        .into_iter()
        .map(|chunk| Bytes::from_static(chunk.as_bytes()))
        .collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (captured_tx, captured_rx) = mpsc::channel(1);
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let service = service_fn(move |request: Request<Incoming>| {
            let captured_tx = captured_tx.clone();
            let chunks = chunks.clone();
            async move {
                let path_and_query = request
                    .uri()
                    .path_and_query()
                    .map_or_else(|| request.uri().path().to_owned(), ToString::to_string);
                let method = request.method().to_string();
                let authorization = request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned);
                let chatgpt_account_id = request
                    .headers()
                    .get("chatgpt-account-id")
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned);
                let x_api_key = request
                    .headers()
                    .get("x-api-key")
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned);
                let anthropic_version = request
                    .headers()
                    .get("anthropic-version")
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned);
                let content_encoding = request
                    .headers()
                    .get("content-encoding")
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned);
                let body = request.into_body().collect().await.unwrap().to_bytes();
                captured_tx
                    .send(CapturedRequest {
                        method,
                        path_and_query,
                        authorization,
                        x_api_key,
                        anthropic_version,
                        chatgpt_account_id,
                        content_encoding,
                        body,
                    })
                    .await
                    .unwrap();

                let response_stream =
                    stream::unfold((chunks, 0_usize), |(chunks, index)| async move {
                        if index >= chunks.len() {
                            return None;
                        }
                        if index > 0 {
                            tokio::time::sleep(Duration::from_millis(250)).await;
                        }
                        let frame = Frame::data(chunks[index].clone());
                        Some((Ok::<_, Infallible>(frame), (chunks, index + 1)))
                    });
                Ok::<_, Infallible>(
                    Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(StreamBody::new(response_stream))
                        .unwrap(),
                )
            }
        });
        hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .await
            .unwrap();
    });
    (address, captured_rx)
}

async fn spawn_anthropic_upstream_with_entity_headers() -> SocketAddr {
    const BODY: &str = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_headers\",\"usage\":{\"input_tokens\":1}}}\n\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"ok\"}}\n\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\ndata: {\"type\":\"message_stop\"}\n\n";
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        hyper::server::conn::http1::Builder::new()
            .serve_connection(
                TokioIo::new(stream),
                service_fn(|request: Request<Incoming>| async move {
                    let _ = request.into_body().collect().await.unwrap();
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(200)
                            .header("content-type", "text/event-stream")
                            .header("content-length", BODY.len())
                            .header("content-encoding", "identity")
                            .header("etag", "upstream-anthropic-entity")
                            .header("last-modified", "Sun, 16 Aug 2026 00:00:00 GMT")
                            .header("content-md5", "upstream-md5")
                            .header("digest", "sha-256=upstream")
                            .body(Full::new(Bytes::from_static(BODY.as_bytes())))
                            .unwrap(),
                    )
                }),
            )
            .await
            .unwrap();
    });
    address
}

async fn spawn_ws_http_bridge_upstream() -> (
    SocketAddr,
    mpsc::Receiver<CapturedRequest>,
    oneshot::Receiver<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (captured_tx, captured_rx) = mpsc::channel(4);
    let (second_request_tx, second_request_rx) = oneshot::channel();
    let request_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let second_request_tx = Arc::new(std::sync::Mutex::new(Some(second_request_tx)));
        let service = service_fn(move |request: Request<Incoming>| {
            let captured_tx = captured_tx.clone();
            let second_request_tx = Arc::clone(&second_request_tx);
            let request_index = request_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                let path_and_query = request
                    .uri()
                    .path_and_query()
                    .map_or_else(|| request.uri().path().to_owned(), ToString::to_string);
                let method = request.method().to_string();
                let authorization = request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned);
                let chatgpt_account_id = request
                    .headers()
                    .get("chatgpt-account-id")
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned);
                let content_encoding = request
                    .headers()
                    .get("content-encoding")
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned);
                let body = request.into_body().collect().await.unwrap().to_bytes();
                captured_tx
                    .send(CapturedRequest {
                        method,
                        path_and_query,
                        authorization,
                        x_api_key: None,
                        anthropic_version: None,
                        chatgpt_account_id,
                        content_encoding,
                        body,
                    })
                    .await
                    .unwrap();

                let events = if request_index == 0 {
                    vec![
                        Bytes::from_static(
                            b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_tool\"}}\n\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"reasoning_tool\",\"summary\":[],\"content\":[]}}\n\ndata: {\"type\":\"response.content_part.added\",\"item_id\":\"reasoning_tool\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"reasoning_text\",\"text\":\"\"}}\n\ndata: {\"type\":\"response.reasoning_text.delta\",\"item_id\":\"reasoning_tool\",\"output_index\":0,\"content_index\":0,\"delta\":\"inspect workspace\"}\n\n",
                        ),
                        Bytes::from_static(
                            b"data: {\"type\":\"response.reasoning_text.done\",\"item_id\":\"reasoning_tool\",\"output_index\":0,\"content_index\":0,\"text\":\"inspect workspace\"}\n\ndata: {\"type\":\"response.content_part.done\",\"item_id\":\"reasoning_tool\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"reasoning_text\",\"text\":\"inspect workspace\"}}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"reasoning_tool\",\"summary\":[],\"content\":[{\"type\":\"reasoning_text\",\"text\":\"inspect workspace\"}]}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_tool\",\"output\":[{\"type\":\"reasoning\",\"id\":\"reasoning_tool\",\"summary\":[],\"content\":[{\"type\":\"reasoning_text\",\"text\":\"inspect workspace\"}]},{\"type\":\"function_call\",\"call_id\":\"call_pwd\",\"name\":\"exec_command\",\"arguments\":\"{\\\"cmd\\\":\\\"pwd\\\"}\"}]}}\n\n",
                        ),
                    ]
                } else {
                    if let Some(sender) = second_request_tx.lock().unwrap().take() {
                        let _ = sender.send(());
                    }
                    vec![Bytes::from_static(
                        b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_done\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}]}}\n\n",
                    )]
                };
                let response_stream =
                    stream::unfold((events, 0_usize), |(events, index)| async move {
                        if index >= events.len() {
                            return None;
                        }
                        if index > 0 {
                            tokio::time::sleep(Duration::from_millis(150)).await;
                        }
                        Some((
                            Ok::<_, Infallible>(Frame::data(events[index].clone())),
                            (events, index + 1),
                        ))
                    });
                Ok::<_, Infallible>(
                    Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(StreamBody::new(response_stream))
                        .unwrap(),
                )
            }
        });
        hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .await
            .unwrap();
    });
    (address, captured_rx, second_request_rx)
}

async fn spawn_chat_bridge_upstream() -> (SocketAddr, mpsc::Receiver<CapturedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (captured_tx, captured_rx) = mpsc::channel(4);
    let request_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let captured_tx = captured_tx.clone();
            let request_index = Arc::clone(&request_index);
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<Incoming>| {
                    let captured_tx = captured_tx.clone();
                    let index = request_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async move {
                        let path_and_query = request.uri().path().to_owned();
                        let method = request.method().to_string();
                        let authorization = request
                            .headers()
                            .get("authorization")
                            .and_then(|value| value.to_str().ok())
                            .map(ToOwned::to_owned);
                        let body = request.into_body().collect().await.unwrap().to_bytes();
                        captured_tx
                            .send(CapturedRequest {
                                method,
                                path_and_query,
                                authorization,
                                x_api_key: None,
                                anthropic_version: None,
                                chatgpt_account_id: None,
                                content_encoding: None,
                                body,
                            })
                            .await
                            .unwrap();
                        let chunks = if index == 0 {
                            vec![Bytes::from_static(b"data: {\"id\":\"chat-tool\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"inspect\",\"tool_calls\":[{\"index\":0,\"id\":\"call_pwd\",\"type\":\"function\",\"function\":{\"name\":\"codex_app__exec_command\",\"arguments\":\"{\\\"cmd\\\":\\\"pwd\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":null}\n\ndata: {\"id\":\"chat-tool\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4,\"total_tokens\":14}}\n\ndata: [DONE]\n\n")]
                        } else {
                            vec![Bytes::from_static(b"data: {\"id\":\"chat-done\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"/tmp\"},\"finish_reason\":\"stop\"}],\"usage\":null}\n\ndata: [DONE]\n\n")]
                        };
                        let body = StreamBody::new(stream::iter(
                            chunks
                                .into_iter()
                                .map(|chunk| Ok::<_, Infallible>(Frame::data(chunk))),
                        ));
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .header("content-type", "text/event-stream")
                                .body(body)
                                .unwrap(),
                        )
                    }
                });
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                    .unwrap();
            });
        }
    });
    (address, captured_rx)
}

async fn spawn_chat_error_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let service = service_fn(move |request: Request<Incoming>| async move {
            let _ = request.into_body().collect().await.unwrap();
            Ok::<_, Infallible>(
                Response::builder()
                    .status(400)
                    .header("content-type", "application/json")
                    .body(Full::new(Bytes::from_static(
                        br#"{"error":{"message":"unsupported tool choice"}}"#,
                    )))
                    .unwrap(),
            )
        });
        hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .await
            .unwrap();
    });
    address
}

async fn spawn_terminal_then_stalled_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let service = service_fn(move |request: Request<Incoming>| async move {
            let _ = request.into_body().collect().await.unwrap();
            let response_stream = stream::unfold(0_u8, |state| async move {
                if state == 0 {
                    return Some((
                        Ok::<_, Infallible>(Frame::data(Bytes::from_static(
                            b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_done\",\"output\":[]}}\n\n",
                        ))),
                        1,
                    ));
                }
                std::future::pending().await
            });
            Ok::<_, Infallible>(
                Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(StreamBody::new(response_stream))
                    .unwrap(),
            )
        });
        hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .await
            .unwrap();
    });
    address
}

struct DropSignal(Option<oneshot::Sender<()>>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

async fn spawn_cancellable_upstream() -> (SocketAddr, oneshot::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (drop_tx, drop_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let drop_tx = Arc::new(std::sync::Mutex::new(Some(drop_tx)));
        let service = service_fn(move |request: Request<Incoming>| {
            let drop_tx = Arc::clone(&drop_tx);
            async move {
                let signal = DropSignal(drop_tx.lock().unwrap().take());
                let _ = request.into_body().collect().await;
                let response_stream =
                    stream::unfold((signal, 0_u64), |(signal, index)| async move {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        let frame = Frame::data(Bytes::from(format!("data: {index}\n\n")));
                        Some((Ok::<_, Infallible>(frame), (signal, index + 1)))
                    });
                Ok::<_, Infallible>(
                    Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(StreamBody::new(response_stream))
                        .unwrap(),
                )
            }
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .await;
    });
    (address, drop_rx)
}

async fn spawn_cancellable_responses_upstream() -> (SocketAddr, oneshot::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (drop_tx, drop_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let drop_tx = Arc::new(std::sync::Mutex::new(Some(drop_tx)));
        let service = service_fn(move |request: Request<Incoming>| {
            let drop_tx = Arc::clone(&drop_tx);
            async move {
                let signal = DropSignal(drop_tx.lock().unwrap().take());
                let _ = request.into_body().collect().await;
                let response_stream = stream::unfold(
                    (signal, 0_u64),
                    |(signal, index)| async move {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        let event = if index == 0 {
                            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_cancel\"}}\n\n".to_owned()
                        } else {
                            format!(
                                "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{index}\"}}\n\n"
                            )
                        };
                        Some((
                            Ok::<_, Infallible>(Frame::data(Bytes::from(event))),
                            (signal, index + 1),
                        ))
                    },
                );
                Ok::<_, Infallible>(
                    Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(StreamBody::new(response_stream))
                        .unwrap(),
                )
            }
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .await;
    });
    (address, drop_rx)
}

async fn spawn_proxy(
    providers: ProvidersDocument,
    official_base_url: String,
) -> (SocketAddr, watch::Sender<bool>) {
    let cache = catalog_cache(&providers);
    spawn_proxy_with_cache_and_observer(providers, cache, official_base_url, None).await
}

async fn spawn_proxy_with_task(
    providers: ProvidersDocument,
    official_base_url: String,
) -> (
    SocketAddr,
    watch::Sender<bool>,
    tokio::task::JoinHandle<Result<(), std::io::Error>>,
) {
    let cache = catalog_cache(&providers);
    let state = Arc::new(
        EgressState::new(&providers, &cache, official_base_url, ingress_capability()).unwrap(),
    );
    let server = EgressServer::bind("127.0.0.1:0".parse().unwrap(), state)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(server.run(shutdown_rx));
    (address, shutdown_tx, task)
}

async fn spawn_proxy_with_observer(
    providers: ProvidersDocument,
    official_base_url: String,
    observer: Option<Arc<dyn EgressObserver>>,
) -> (SocketAddr, watch::Sender<bool>) {
    let cache = catalog_cache(&providers);
    spawn_proxy_with_cache_and_observer(providers, cache, official_base_url, observer).await
}

async fn spawn_proxy_with_cache(
    providers: ProvidersDocument,
    cache: ModelCacheDocument,
    official_base_url: String,
) -> (SocketAddr, watch::Sender<bool>) {
    spawn_proxy_with_cache_and_observer(providers, cache, official_base_url, None).await
}

async fn spawn_proxy_with_cache_and_observer(
    providers: ProvidersDocument,
    cache: ModelCacheDocument,
    official_base_url: String,
    observer: Option<Arc<dyn EgressObserver>>,
) -> (SocketAddr, watch::Sender<bool>) {
    let mut state =
        EgressState::new(&providers, &cache, official_base_url, ingress_capability()).unwrap();
    if let Some(observer) = observer {
        state = state.with_observer(observer);
    }
    let state = Arc::new(state);
    let server = EgressServer::bind("127.0.0.1:0".parse().unwrap(), state)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(server.run(shutdown_rx));
    (address, shutdown_tx)
}

fn client() -> Client<HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

#[tokio::test]
async fn official_request_preserves_body_and_credentials_and_streams_sse() {
    let (upstream, mut captured) =
        spawn_upstream(vec!["data: first\n\n", "data: second\n\n"]).await;
    let (proxy, shutdown) = spawn_proxy(
        providers(None, 1_048_576),
        format!("http://{upstream}/backend-api/codex"),
    )
    .await;
    let original = Bytes::from_static(br#"{ "model": "gpt-5.6", "input": "hello" }"#);
    let request = Request::builder()
        .method("POST")
        .uri(ingress_http_url(proxy, "/v1/responses?stream=true"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer official-secret")
        .header("chatgpt-account-id", "account")
        .body(Full::new(original.clone()))
        .unwrap();

    let response = client().request(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let mut body = response.into_body();
    let first = tokio::time::timeout(Duration::from_millis(500), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
    assert_eq!(first, "data: first\n\n");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), body.frame())
            .await
            .is_err()
    );
    let second = tokio::time::timeout(Duration::from_millis(500), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
    assert_eq!(second, "data: second\n\n");

    let captured = captured.recv().await.unwrap();
    assert_eq!(captured.method, "POST");
    assert_eq!(
        captured.path_and_query,
        "/backend-api/codex/responses?stream=true"
    );
    assert_eq!(
        captured.authorization.as_deref(),
        Some("Bearer official-secret")
    );
    assert_eq!(captured.chatgpt_account_id.as_deref(), Some("account"));
    assert_eq!(captured.body, original);
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn official_zstd_request_preserves_the_original_compressed_bytes() {
    let (upstream, mut captured) = spawn_upstream(vec!["data: done\n\n"]).await;
    let (proxy, shutdown) = spawn_proxy(
        providers(None, 1_048_576),
        format!("http://{upstream}/backend-api/codex"),
    )
    .await;
    let original = serde_json::json!({
        "model": "gpt-5.6-sol",
        "input": "hello",
        "client_metadata": {"contract": true}
    });
    let compressed = Bytes::from(
        zstd::stream::encode_all(
            std::io::Cursor::new(serde_json::to_vec(&original).unwrap()),
            0,
        )
        .unwrap(),
    );
    let request = Request::builder()
        .method("POST")
        .uri(ingress_http_url(proxy, "/v1/responses"))
        .header("content-type", "application/json")
        .header("content-encoding", "zstd")
        .header("authorization", "Bearer official-secret")
        .body(Full::new(compressed.clone()))
        .unwrap();

    assert_eq!(client().request(request).await.unwrap().status(), 200);
    let captured = captured.recv().await.unwrap();
    assert_eq!(captured.content_encoding.as_deref(), Some("zstd"));
    assert_eq!(captured.body, compressed);
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn official_model_catalog_request_is_transparently_proxied() {
    let (upstream, mut captured) = spawn_upstream(vec![r#"{"models":[]}"#]).await;
    let (proxy, shutdown) = spawn_proxy(
        providers(None, 1_048_576),
        format!("http://{upstream}/backend-api/codex"),
    )
    .await;
    let request = Request::builder()
        .method("GET")
        .uri(ingress_http_url(proxy, "/v1/models?client_version=0.147.0"))
        .header("authorization", "Bearer official-secret")
        .body(Full::new(Bytes::new()))
        .unwrap();

    let response = client().request(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let captured = captured.recv().await.unwrap();
    assert_eq!(captured.method, "GET");
    assert_eq!(
        captured.path_and_query,
        "/backend-api/codex/models?client_version=0.147.0"
    );
    assert_eq!(
        captured.authorization.as_deref(),
        Some("Bearer official-secret")
    );
    assert!(captured.body.is_empty());
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn official_search_request_is_transparently_proxied() {
    let (official, mut official_captured) = spawn_upstream(vec![
        r#"{"results":[{"type":"computer_initialize_state"}]}"#,
    ])
    .await;
    let (third_party, mut third_party_captured) = spawn_upstream(vec!["unexpected"]).await;
    let configured = provider(third_party);
    let (proxy, shutdown) = spawn_proxy(
        providers(Some(configured), 1_048_576),
        format!("http://{official}/backend-api/codex"),
    )
    .await;
    let original = Bytes::from_static(
        br#"{"id":"session","model":"provider-a/coder","commands":{"search_query":[{"q":"Codex documentation"}]}}"#,
    );
    let request = Request::builder()
        .method("POST")
        .uri(ingress_http_url(
            proxy,
            "/v1/alpha/search?client_version=0.147.0",
        ))
        .header("content-type", "application/json")
        .header("authorization", "Bearer official-secret")
        .header("chatgpt-account-id", "account")
        .body(Full::new(original.clone()))
        .unwrap();

    let response = client().request(request).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        r#"{"results":[{"type":"computer_initialize_state"}]}"#
    );
    let captured = official_captured.recv().await.unwrap();
    assert_eq!(captured.method, "POST");
    assert_eq!(
        captured.path_and_query,
        "/backend-api/codex/alpha/search?client_version=0.147.0"
    );
    assert_eq!(
        captured.authorization.as_deref(),
        Some("Bearer official-secret")
    );
    assert_eq!(captured.chatgpt_account_id.as_deref(), Some("account"));
    assert_eq!(captured.body, original);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), third_party_captured.recv())
            .await
            .is_err(),
        "standalone search must never be routed to a private Provider endpoint"
    );
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn other_official_api_methods_and_paths_are_transparently_proxied() {
    let (official, mut captured) = spawn_upstream(vec![r#"{"deleted":true}"#]).await;
    let (proxy, shutdown) = spawn_proxy(
        providers(None, 1_048_576),
        format!("http://{official}/backend-api/codex"),
    )
    .await;
    let request = Request::builder()
        .method("DELETE")
        .uri(ingress_http_url(
            proxy,
            "/v1/responses/response-1?reason=user_requested",
        ))
        .header("authorization", "Bearer official-secret")
        .header("chatgpt-account-id", "account")
        .body(Full::new(Bytes::new()))
        .unwrap();

    let response = client().request(request).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        r#"{"deleted":true}"#
    );
    let captured = captured.recv().await.unwrap();
    assert_eq!(captured.method, "DELETE");
    assert_eq!(
        captured.path_and_query,
        "/backend-api/codex/responses/response-1?reason=user_requested"
    );
    assert_eq!(
        captured.authorization.as_deref(),
        Some("Bearer official-secret")
    );
    assert_eq!(captured.chatgpt_account_id.as_deref(), Some("account"));
    assert!(captured.body.is_empty());
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn upstream_connection_errors_emit_redacted_request_diagnostics() {
    let observer = Arc::new(CollectingObserver::default());
    let contract_observer: Arc<dyn EgressObserver> = observer.clone();
    let (proxy, shutdown) = spawn_proxy_with_observer(
        providers(None, 1_048_576),
        "http://127.0.0.1:9/backend-api/codex".to_owned(),
        Some(contract_observer),
    )
    .await;
    let request = Request::builder()
        .method("GET")
        .uri(ingress_http_url(
            proxy,
            "/v1/responses/response-1?account_id=must-not-be-logged",
        ))
        .body(Full::new(Bytes::new()))
        .unwrap();

    let response = client().request(request).await.unwrap();
    assert_eq!(response.status(), hyper::StatusCode::BAD_GATEWAY);
    assert!(observer.records().iter().any(|event| matches!(
        event,
        EgressEvent::ErrorObserved(error)
            if error.method == "GET"
                && error.path == "/v1/responses/response-1"
                && error.ingress_authorized
                && error.status == Some(502)
                && error.code == "upstream_connect_failed"
                && !error.message.contains("must-not-be-logged")
    )));
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn official_model_catalog_is_merged_locally_without_contacting_third_party() {
    let (official, mut official_captured) = spawn_upstream(vec![
        r#"{"models":[{"slug":"gpt-official","opaque":{"kept":true},"model_messages":{"instructions_variables":{"personality_default":"default personality","personality_friendly":"friendly personality","personality_pragmatic":"pragmatic personality"}}}]}"#,
    ])
    .await;
    let (third_party, mut third_party_captured) = spawn_upstream(vec!["unexpected"]).await;
    let configured = provider(third_party);
    let (proxy, shutdown) = spawn_proxy(
        providers(Some(configured), 1_048_576),
        format!("http://{official}/backend-api/codex"),
    )
    .await;
    let request = Request::builder()
        .method("GET")
        .uri(ingress_http_url(proxy, "/v1/models?client_version=0.147.0"))
        .header("authorization", "Bearer official-secret")
        .header("chatgpt-account-id", "account")
        .body(Full::new(Bytes::new()))
        .unwrap();

    let response = client().request(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&body).unwrap();
    let models = body["models"].as_array().unwrap();
    assert!(models.iter().any(|model| model["slug"] == "gpt-official"));
    assert!(
        models
            .iter()
            .any(|model| model["slug"] == "provider-a/coder")
    );
    let private_model = models
        .iter()
        .find(|model| model["slug"] == "provider-a/coder")
        .unwrap();
    assert_eq!(private_model["multi_agent_version"], "v2");
    assert_eq!(
        private_model["base_instructions"],
        "You are Coder. Your provider-x model identifier is provider-a/coder.\n\ndefault personality\n\nFollow the user's request and use the provided tools when needed."
    );
    assert_eq!(
        private_model["model_messages"]["instructions_template"],
        "You are Coder. Your provider-x model identifier is provider-a/coder.\n\n{{ personality }}\n\nFollow the user's request and use the provided tools when needed."
    );
    assert_eq!(
        private_model["model_messages"]["instructions_variables"]["personality_pragmatic"],
        "pragmatic personality"
    );
    for field in [
        "approvals",
        "collaboration_modes",
        "auto_review",
        "multi_agent",
        "permissions",
        "token_budget",
    ] {
        assert_eq!(private_model["model_messages"][field], Value::Null);
    }
    assert_eq!(body["models"][0]["opaque"]["kept"], true);

    let captured = official_captured.recv().await.unwrap();
    assert_eq!(
        captured.authorization.as_deref(),
        Some("Bearer official-secret")
    );
    assert_eq!(captured.chatgpt_account_id.as_deref(), Some("account"));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), third_party_captured.recv())
            .await
            .is_err(),
        "catalog merge must never send the official request to a Provider endpoint"
    );
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn third_party_request_rewrites_only_model_and_replaces_credentials() {
    let (upstream, mut captured) = spawn_upstream(vec!["data: done\n\n"]).await;
    let configured = provider(upstream);
    let (proxy, shutdown) = spawn_proxy(
        providers(Some(configured), 1_048_576),
        "http://127.0.0.1:9/backend-api/codex".to_owned(),
    )
    .await;
    let original = serde_json::json!({
        "model": "provider-a/coder",
        "input": "hello",
        "nested": {"model": "unchanged"},
        "client_metadata": {"x-openai-subagent": "review"}
    });
    let request = Request::builder()
        .method("POST")
        .uri(ingress_http_url(proxy, "/v1/responses"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer official-secret")
        .header("chatgpt-account-id", "account")
        .header("x-openai-attestation", "proof")
        .body(Full::new(Bytes::from(
            serde_json::to_vec(&original).unwrap(),
        )))
        .unwrap();

    let response = client().request(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let captured = captured.recv().await.unwrap();
    assert_eq!(captured.path_and_query, "/v1/responses");
    assert_eq!(
        captured.authorization.as_deref(),
        Some("Bearer third-party-secret")
    );
    assert!(captured.chatgpt_account_id.is_none());
    let rewritten: Value = serde_json::from_slice(&captured.body).unwrap();
    assert_eq!(rewritten["model"], "coder");
    assert_eq!(rewritten["nested"], original["nested"]);
    assert_eq!(rewritten["client_metadata"], original["client_metadata"]);
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn chat_completions_provider_converts_http_request_and_sse_response() {
    let (upstream, mut captured) = spawn_chat_bridge_upstream().await;
    let configured = chat_provider(upstream);
    let (proxy, shutdown) = spawn_proxy(
        providers(Some(configured), 1_048_576),
        "http://127.0.0.1:9/backend-api/codex".to_owned(),
    )
    .await;
    let request = Request::builder()
        .method("POST")
        .uri(ingress_http_url(proxy, "/v1/responses"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer official-secret")
        .header("chatgpt-account-id", "account")
        .body(Full::new(Bytes::from_static(
            br#"{"model":"provider-a/coder","instructions":"Use tools","input":"pwd","tools":[{"type":"function","name":"exec_command","parameters":{"type":"object"}}]}"#,
        )))
        .unwrap();

    let response = client().request(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let downstream = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(downstream.contains("response.function_call_arguments.delta"));
    assert!(downstream.contains("response.completed"));

    let captured = captured.recv().await.unwrap();
    assert_eq!(captured.path_and_query, "/v1/chat/completions");
    assert_eq!(
        captured.authorization.as_deref(),
        Some("Bearer third-party-secret")
    );
    let converted: Value = serde_json::from_slice(&captured.body).unwrap();
    assert_eq!(converted["model"], "coder");
    assert_eq!(converted["messages"][0]["content"], "Use tools");
    assert_eq!(converted["messages"][1]["content"], "pwd");
    assert_eq!(converted["tools"][0]["function"]["name"], "exec_command");
    assert_eq!(converted["stream"], true);
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn anthropic_provider_converts_http_request_headers_and_sse_response() {
    let (upstream, mut captured) = spawn_upstream(vec![
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"content\":[],\"usage\":{\"input_tokens\":4,\"output_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ])
    .await;
    let configured = anthropic_provider(upstream);
    let (proxy, shutdown) = spawn_proxy(
        providers(Some(configured), 1_048_576),
        "http://127.0.0.1:9/backend-api/codex".to_owned(),
    )
    .await;
    let request = Request::builder()
        .method("POST")
        .uri(ingress_http_url(proxy, "/v1/responses"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer official-secret")
        .body(Full::new(Bytes::from_static(
            br#"{"model":"provider-a/coder","instructions":"Be exact","input":"ping","max_output_tokens":2048,"tools":[{"type":"function","name":"marker","parameters":{"type":"object"}}]}"#,
        )))
        .unwrap();

    let response = client().request(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let downstream = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(downstream.contains("response.output_text.delta"));
    assert!(downstream.contains("response.completed"));

    let captured = captured.recv().await.unwrap();
    assert_eq!(captured.path_and_query, "/v1/messages");
    assert!(captured.authorization.is_none());
    assert_eq!(captured.x_api_key.as_deref(), Some("third-party-secret"));
    assert_eq!(captured.anthropic_version.as_deref(), Some("2023-06-01"));
    let converted: Value = serde_json::from_slice(&captured.body).unwrap();
    assert_eq!(converted["model"], "coder");
    assert_eq!(converted["system"], "Be exact");
    assert_eq!(converted["messages"][0]["content"], "ping");
    assert_eq!(converted["tools"][0]["name"], "marker");
    assert_eq!(converted["stream"], true);
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn anthropic_success_rewrite_removes_upstream_entity_headers() {
    let upstream = spawn_anthropic_upstream_with_entity_headers().await;
    let configured = anthropic_provider(upstream);
    let (proxy, shutdown) = spawn_proxy(
        providers(Some(configured), 1_048_576),
        "http://127.0.0.1:9/backend-api/codex".to_owned(),
    )
    .await;
    let request = Request::builder()
        .method("POST")
        .uri(ingress_http_url(proxy, "/v1/responses"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from_static(
            br#"{"model":"provider-a/coder","input":"ping"}"#,
        )))
        .unwrap();

    let response = client().request(request).await.unwrap();
    assert_eq!(response.status(), 200);
    for name in [
        "content-length",
        "content-encoding",
        "etag",
        "last-modified",
        "content-md5",
        "digest",
    ] {
        assert!(
            !response.headers().contains_key(name),
            "{name} was retained"
        );
    }
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("response.completed"));
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn chat_completions_http_preserves_upstream_error_body() {
    let upstream = spawn_chat_error_upstream().await;
    let configured = chat_provider(upstream);
    let (proxy, shutdown) = spawn_proxy(
        providers(Some(configured), 1_048_576),
        "http://127.0.0.1:9/backend-api/codex".to_owned(),
    )
    .await;
    let request = Request::builder()
        .method("POST")
        .uri(ingress_http_url(proxy, "/v1/responses"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from_static(
            br#"{"model":"provider-a/coder","input":"hello"}"#,
        )))
        .unwrap();

    let response = client().request(request).await.unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(response.headers()["content-type"], "application/json");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        body,
        Bytes::from_static(br#"{"error":{"message":"unsupported tool choice"}}"#)
    );
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn third_party_zstd_request_is_normalized_to_identity_json() {
    let (upstream, mut captured) = spawn_upstream(vec!["data: done\n\n"]).await;
    let configured = provider(upstream);
    let (proxy, shutdown) = spawn_proxy(
        providers(Some(configured), 1_048_576),
        "http://127.0.0.1:9/backend-api/codex".to_owned(),
    )
    .await;
    let original = serde_json::json!({
        "model": "provider-a/coder",
        "input": "hello",
        "nested": {"model": "unchanged"}
    });
    let compressed = zstd::stream::encode_all(
        std::io::Cursor::new(serde_json::to_vec(&original).unwrap()),
        0,
    )
    .unwrap();
    let request = Request::builder()
        .method("POST")
        .uri(ingress_http_url(proxy, "/v1/responses"))
        .header("content-type", "application/json")
        .header("content-encoding", "zstd")
        .header("authorization", "Bearer official-secret")
        .body(Full::new(Bytes::from(compressed)))
        .unwrap();

    assert_eq!(client().request(request).await.unwrap().status(), 200);
    let captured = captured.recv().await.unwrap();
    assert_eq!(captured.content_encoding, None);
    let rewritten: Value = serde_json::from_slice(&captured.body).unwrap();
    assert_eq!(rewritten["model"], "coder");
    assert_eq!(rewritten["nested"], original["nested"]);
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn oversized_and_unavailable_managed_requests_fail_locally() {
    let (proxy, shutdown) = spawn_proxy(
        providers(None, 80),
        "http://127.0.0.1:9/backend-api/codex".to_owned(),
    )
    .await;

    let oversized = Request::builder()
        .method("POST")
        .uri(ingress_http_url(proxy, "/v1/responses"))
        .body(Full::new(Bytes::from_static(
            br#"{"model":"gpt-5.6","input":"this request body is deliberately much longer than the configured request limit so it must fail locally"}"#,
        )))
        .unwrap();
    assert_eq!(
        client().request(oversized).await.unwrap().status(),
        hyper::StatusCode::PAYLOAD_TOO_LARGE
    );

    let unavailable = Request::builder()
        .method("POST")
        .uri(ingress_http_url(proxy, "/v1/responses"))
        .body(Full::new(Bytes::from_static(
            br#"{"model":"disabled/coder","input":"hello"}"#,
        )))
        .unwrap();
    assert_eq!(
        client().request(unavailable).await.unwrap().status(),
        hyper::StatusCode::NOT_FOUND
    );
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn idle_upstream_stream_is_terminated_without_buffering_the_whole_response() {
    let (upstream, _captured) = spawn_upstream(vec!["data: first\n\n", "data: late\n\n"]).await;
    let mut configuration = providers(None, 1_048_576);
    configuration.timeouts.stream_idle_ms = 50;
    let (proxy, shutdown) = spawn_proxy(
        configuration,
        format!("http://{upstream}/backend-api/codex"),
    )
    .await;
    let request = Request::builder()
        .method("POST")
        .uri(ingress_http_url(proxy, "/v1/responses"))
        .body(Full::new(Bytes::from_static(
            br#"{"model":"gpt-5.6","input":"hello"}"#,
        )))
        .unwrap();

    let response = client().request(request).await.unwrap();
    let mut body = response.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(first, "data: first\n\n");
    let second = tokio::time::timeout(Duration::from_millis(300), body.frame())
        .await
        .unwrap()
        .unwrap();
    assert!(second.is_err());
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn server_refuses_non_loopback_bind_addresses() {
    let configuration = providers(None, 1_048_576);
    let state = Arc::new(
        EgressState::new(
            &configuration,
            &catalog_cache(&configuration),
            "http://127.0.0.1:9/backend-api/codex",
            ingress_capability(),
        )
        .unwrap(),
    );
    let error = EgressServer::bind("0.0.0.0:0".parse().unwrap(), state)
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[tokio::test]
async fn server_uses_next_available_port_in_managed_range() {
    let occupied = loop {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        if listener.local_addr().unwrap().port() <= u16::MAX - 10 {
            break listener;
        }
    };
    let first_port = occupied.local_addr().unwrap().port();
    let configuration = providers(None, 1_048_576);
    let state = Arc::new(
        EgressState::new(
            &configuration,
            &catalog_cache(&configuration),
            "http://127.0.0.1:9/backend-api/codex",
            ingress_capability(),
        )
        .unwrap(),
    );

    let server = EgressServer::bind_first_available(
        "127.0.0.1".parse().unwrap(),
        first_port..=first_port + 10,
        state,
    )
    .await
    .unwrap();

    let selected = server.local_addr().unwrap().port();
    assert!(selected > first_port);
    assert!(selected <= first_port + 10);
}

#[tokio::test]
async fn active_websocket_holds_the_only_connection_permit_until_close() {
    let mut configuration = providers(None, 1_048_576);
    configuration.listener.max_connections = 1;
    let (upstream, mut captured) = spawn_upstream(vec![r#"{"deleted":true}"#]).await;
    let (proxy, shutdown) = spawn_proxy(
        configuration,
        format!("http://{upstream}/backend-api/codex"),
    )
    .await;
    let (mut websocket, _) = connect_async(ingress_websocket_url(proxy)).await.unwrap();

    let request = Request::builder()
        .method("DELETE")
        .uri(ingress_http_url(proxy, "/v1/responses"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let http_client = client();
    let mut second_connection =
        tokio::spawn(async move { http_client.request(request).await.unwrap() });

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !second_connection.is_finished(),
        "the upgraded WebSocket released the only connection permit"
    );

    websocket.close(None).await.unwrap();
    drop(websocket);
    let response = tokio::time::timeout(Duration::from_secs(2), &mut second_connection)
        .await
        .expect("the second connection did not resume after the WebSocket closed")
        .unwrap();
    assert_eq!(response.status(), hyper::StatusCode::OK);
    assert_eq!(captured.recv().await.unwrap().method, "DELETE");
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn requests_without_the_exact_capability_are_rejected_before_routing() {
    let observer = Arc::new(CollectingObserver::default());
    let contract_observer: Arc<dyn EgressObserver> = observer.clone();
    let (proxy, shutdown) = spawn_proxy_with_observer(
        providers(None, 1_048_576),
        "http://127.0.0.1:9/backend-api/codex".to_owned(),
        Some(contract_observer),
    )
    .await;

    for path in [
        "/v1/responses",
        "/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff/v1/responses",
        "/not-an-api-path",
    ] {
        let request = Request::builder()
            .method("POST")
            .uri(format!("http://{proxy}{path}"))
            .body(Full::new(Bytes::from_static(
                br#"{"model":"gpt-5.6","input":"hello"}"#,
            )))
            .unwrap();
        let response = client().request(request).await.unwrap();
        assert_eq!(response.status(), hyper::StatusCode::NOT_FOUND);
    }
    let records = observer.records();
    assert_eq!(records.len(), 3);
    let errors: Vec<_> = records
        .iter()
        .filter_map(|event| match event {
            EgressEvent::ErrorObserved(error) => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 3);
    assert_eq!(errors[0].path, "/v1/responses");
    assert_eq!(errors[1].path, "/<redacted-capability>/v1/responses");
    assert!(!errors[1].path.contains("ffffffffffffffff"));
    assert_eq!(errors[2].path, "/not-an-api-path");
    assert!(
        errors
            .iter()
            .all(|error| error.code == "ingress_not_found" && !error.ingress_authorized)
    );
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn downstream_cancellation_drops_the_upstream_stream() {
    let (upstream, dropped) = spawn_cancellable_upstream().await;
    let (proxy, shutdown) = spawn_proxy(
        providers(None, 1_048_576),
        format!("http://{upstream}/backend-api/codex"),
    )
    .await;
    let request = Request::builder()
        .method("POST")
        .uri(ingress_http_url(proxy, "/v1/responses"))
        .body(Full::new(Bytes::from_static(
            br#"{"model":"gpt-5.6","input":"hello"}"#,
        )))
        .unwrap();

    let response = client().request(request).await.unwrap();
    let mut body = response.into_body();
    assert!(body.frame().await.unwrap().is_ok());
    drop(body);

    tokio::time::timeout(Duration::from_secs(2), dropped)
        .await
        .expect("upstream body was not cancelled")
        .unwrap();
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn websocket_upgrade_can_signal_http_fallback_for_contract_probes() {
    let observer = Arc::new(CollectingObserver::default());
    let contract_observer: Arc<dyn EgressObserver> = observer.clone();
    let configuration = providers(None, 1_048_576);
    let state = EgressState::new(
        &configuration,
        &catalog_cache(&configuration),
        "http://127.0.0.1:9/backend-api/codex",
        ingress_capability(),
    )
    .unwrap()
    .with_observer(contract_observer)
    .with_websocket_fallback_on_upgrade();
    let server = EgressServer::bind("127.0.0.1:0".parse().unwrap(), Arc::new(state))
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(server.run(shutdown_rx));

    let error = connect_async(ingress_websocket_url(address))
        .await
        .unwrap_err();
    let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
        panic!("expected HTTP handshake error");
    };
    assert_eq!(response.status(), hyper::StatusCode::UPGRADE_REQUIRED);
    assert!(observer.records().iter().any(|event| matches!(
        event,
        EgressEvent::FallbackObserved(event)
            if event.transport == ObservedTransport::WebSocket
                && event.path == "/v1/responses"
                && event.status == 426
    )));
    shutdown_tx.send(true).unwrap();
}

#[tokio::test]
async fn websocket_upgrade_rejects_browser_origins_before_connecting_upstream() {
    let configuration = providers(None, 1_048_576);
    let state = EgressState::new(
        &configuration,
        &catalog_cache(&configuration),
        "http://127.0.0.1:9/backend-api/codex",
        ingress_capability(),
    )
    .unwrap();
    let server = EgressServer::bind("127.0.0.1:0".parse().unwrap(), Arc::new(state))
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(server.run(shutdown_rx));

    let mut request = ingress_websocket_url(address)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("origin", "https://evil.example".parse().unwrap());
    let error = connect_async(request).await.unwrap_err();
    let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
        panic!("expected HTTP handshake error");
    };
    assert_eq!(response.status(), hyper::StatusCode::FORBIDDEN);
    shutdown_tx.send(true).unwrap();
}

#[tokio::test]
async fn official_websocket_preserves_credentials_and_supports_multiple_turns() {
    let (upstream, mut captured, upstream_closed) = spawn_websocket_upstream().await;
    let observer = Arc::new(CollectingObserver::default());
    let contract_observer: Arc<dyn EgressObserver> = observer.clone();
    let (proxy, shutdown) = spawn_proxy_with_observer(
        providers(None, 1_048_576),
        format!("http://{upstream}/backend-api/codex"),
        Some(contract_observer),
    )
    .await;
    let mut request = ingress_websocket_url(proxy).into_client_request().unwrap();
    request
        .headers_mut()
        .insert("authorization", "Bearer official-secret".parse().unwrap());
    request
        .headers_mut()
        .insert("chatgpt-account-id", "account".parse().unwrap());
    let (mut websocket, _) = connect_async(request).await.unwrap();

    let first = serde_json::json!({
        "type": "response.create",
        "model": "gpt-5.6",
        "input": "first"
    });
    websocket
        .send(Message::text(first.to_string()))
        .await
        .unwrap();
    assert!(matches!(
        websocket.next().await.unwrap().unwrap(),
        Message::Text(_)
    ));
    let first_captured = captured.recv().await.unwrap();
    assert_eq!(first_captured.path, "/backend-api/codex/responses");
    assert_eq!(
        first_captured.authorization.as_deref(),
        Some("Bearer official-secret")
    );
    assert_eq!(
        first_captured.chatgpt_account_id.as_deref(),
        Some("account")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&first_captured.text).unwrap(),
        first
    );

    let second = serde_json::json!({
        "type": "response.create",
        "model": "gpt-5.6",
        "previous_response_id": "resp_test",
        "input": "second"
    });
    websocket
        .send(Message::text(second.to_string()))
        .await
        .unwrap();
    assert!(matches!(
        websocket.next().await.unwrap().unwrap(),
        Message::Text(_)
    ));
    let second_captured = captured.recv().await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&second_captured.text).unwrap(),
        second
    );

    websocket.close(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), upstream_closed)
        .await
        .unwrap()
        .unwrap();
    let records = observer.records();
    let request_records: Vec<_> = records
        .iter()
        .filter_map(|event| match event {
            EgressEvent::RequestObserved(event) => Some(event),
            EgressEvent::UpstreamObserved(_)
            | EgressEvent::FallbackObserved(_)
            | EgressEvent::ErrorObserved(_) => None,
        })
        .collect();
    assert_eq!(request_records.len(), 2);
    assert_eq!(request_records[0].transport, ObservedTransport::WebSocket);
    assert_eq!(request_records[0].route, ObservedRoute::Official);
    assert_eq!(request_records[0].sequence, 1);
    assert!(!request_records[0].previous_response_id_present);
    assert_eq!(request_records[1].sequence, 2);
    assert!(request_records[1].previous_response_id_present);
    assert!(records.iter().any(|event| matches!(
        event,
        EgressEvent::UpstreamObserved(event)
            if event.transport == ObservedTransport::WebSocket
                && event.route == ObservedRoute::Official
                && event.status == 101
    )));
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn direct_websocket_rejects_a_concurrent_response_create() {
    let (upstream, upstream_closed) =
        spawn_delayed_websocket_upstream(Duration::from_millis(500)).await;
    let (proxy, shutdown) = spawn_proxy(
        providers(None, 1_048_576),
        format!("http://{upstream}/backend-api/codex"),
    )
    .await;
    let (mut websocket, _) = connect_async(ingress_websocket_url(proxy)).await.unwrap();

    websocket
        .send(Message::text(
            serde_json::json!({
                "type": "response.create",
                "model": "gpt-5.6",
                "input": "first"
            })
            .to_string(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        websocket.next().await.unwrap().unwrap(),
        Message::Text(_)
    ));

    websocket
        .send(Message::text(
            serde_json::json!({
                "type": "response.create",
                "model": "gpt-5.6",
                "input": "must be rejected"
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let error = tokio::time::timeout(Duration::from_millis(200), websocket.next())
        .await
        .expect("concurrent response.create was forwarded upstream")
        .unwrap()
        .unwrap();
    let Message::Text(error) = error else {
        panic!("expected an explicit concurrent_request event");
    };
    let error: Value = serde_json::from_str(error.as_ref()).unwrap();
    assert_eq!(error["error"]["code"], "concurrent_request");
    assert!(matches!(
        websocket.next().await.unwrap().unwrap(),
        Message::Close(_)
    ));
    tokio::time::timeout(Duration::from_secs(2), upstream_closed)
        .await
        .unwrap()
        .unwrap();
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn third_party_websocket_rewrites_model_and_rejects_route_switching() {
    let (upstream, mut captured, upstream_closed) = spawn_websocket_upstream().await;
    let configured = websocket_provider(upstream);
    let (proxy, shutdown) = spawn_proxy(
        providers(Some(configured), 1_048_576),
        "http://127.0.0.1:9/backend-api/codex".to_owned(),
    )
    .await;
    let mut request = ingress_websocket_url(proxy).into_client_request().unwrap();
    request
        .headers_mut()
        .insert("authorization", "Bearer official-secret".parse().unwrap());
    request
        .headers_mut()
        .insert("chatgpt-account-id", "account".parse().unwrap());
    let (mut websocket, _) = connect_async(request).await.unwrap();

    websocket
        .send(Message::text(
            serde_json::json!({
                "type": "response.create",
                "model": "provider-a/coder",
                "input": "first",
                "client_metadata": {"x-openai-subagent": "review"}
            })
            .to_string(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        websocket.next().await.unwrap().unwrap(),
        Message::Text(_)
    ));
    let first_captured = captured.recv().await.unwrap();
    assert_eq!(first_captured.path, "/v1/responses");
    assert_eq!(
        first_captured.authorization.as_deref(),
        Some("Bearer third-party-secret")
    );
    assert!(first_captured.chatgpt_account_id.is_none());
    let first_body: Value = serde_json::from_str(&first_captured.text).unwrap();
    assert_eq!(first_body["model"], "coder");
    assert_eq!(
        first_body["client_metadata"],
        serde_json::json!({"x-openai-subagent": "review"})
    );

    websocket
        .send(Message::text(
            serde_json::json!({
                "type": "response.create",
                "model": "provider-a/coder",
                "previous_response_id": "resp_test",
                "input": "same provider"
            })
            .to_string(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        websocket.next().await.unwrap().unwrap(),
        Message::Text(_)
    ));
    let second_captured = captured.recv().await.unwrap();
    let second_body: Value = serde_json::from_str(&second_captured.text).unwrap();
    assert_eq!(second_body["model"], "coder");
    assert_eq!(second_body["previous_response_id"], "resp_test");

    websocket
        .send(Message::text(
            serde_json::json!({
                "type": "response.create",
                "model": "gpt-5.6",
                "previous_response_id": "resp_test",
                "input": "must not cross providers"
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let error = websocket.next().await.unwrap().unwrap();
    let Message::Text(error) = error else {
        panic!("expected an explicit route_changed event");
    };
    let error: Value = serde_json::from_str(error.as_ref()).unwrap();
    assert_eq!(error["error"]["code"], "route_changed");
    assert!(matches!(
        websocket.next().await.unwrap().unwrap(),
        Message::Close(_)
    ));
    assert!(captured.try_recv().is_err());
    tokio::time::timeout(Duration::from_secs(2), upstream_closed)
        .await
        .unwrap()
        .unwrap();
    shutdown.send(true).unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One end-to-end test keeps the two tool rounds on one WebSocket.
async fn http_only_provider_bridges_websocket_to_stateful_http_responses() {
    let (upstream, mut captured, second_request) = spawn_ws_http_bridge_upstream().await;
    let configured = provider(upstream);
    let (proxy, shutdown) = spawn_proxy(
        providers(Some(configured), 1_048_576),
        "http://127.0.0.1:9/backend-api/codex".to_owned(),
    )
    .await;
    let request = ingress_websocket_url(proxy).into_client_request().unwrap();
    let (mut websocket, _) = connect_async(request).await.unwrap();
    websocket
        .send(Message::text(
            serde_json::json!({
                "type": "response.create",
                "model": "provider-a/coder",
                "generate": false,
                "instructions": "Use the supplied tools",
                "tools": [{
                    "type": "function",
                    "name": "exec_command",
                    "parameters": {"type": "object"}
                }]
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let Message::Text(created) = websocket.next().await.unwrap().unwrap() else {
        panic!("expected local warmup response.created")
    };
    let warmup_id = serde_json::from_str::<Value>(created.as_ref()).unwrap()["response"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(matches!(
        websocket.next().await.unwrap().unwrap(),
        Message::Text(_)
    ));
    assert!(captured.try_recv().is_err(), "warmup must stay local");

    websocket
        .send(Message::text(
            serde_json::json!({
                "type": "response.create",
                "model": "provider-a/coder",
                "previous_response_id": warmup_id,
                "input": [{"type": "message", "role": "user", "content": "pwd"}]
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let first_event = tokio::time::timeout(Duration::from_millis(140), websocket.next())
        .await
        .expect("the first SSE event must be forwarded before completion")
        .unwrap()
        .unwrap();
    let Message::Text(first_event) = first_event else {
        panic!("expected text event")
    };
    let mut downstream_events = vec![serde_json::from_str::<Value>(first_event.as_ref()).unwrap()];
    while downstream_events.last().unwrap()["type"] != "response.completed" {
        let Message::Text(event) = websocket.next().await.unwrap().unwrap() else {
            panic!("expected text event")
        };
        downstream_events.push(serde_json::from_str(event.as_ref()).unwrap());
    }
    let event_types: Vec<_> = downstream_events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect();
    assert!(event_types.contains(&"response.reasoning_summary_part.added"));
    assert!(event_types.contains(&"response.reasoning_summary_text.delta"));
    assert!(event_types.contains(&"response.reasoning_summary_text.done"));
    assert!(!event_types.contains(&"response.reasoning_text.delta"));
    let reasoning_done = downstream_events
        .iter()
        .find(|event| {
            event["type"] == "response.output_item.done" && event["item"]["type"] == "reasoning"
        })
        .unwrap();
    assert_eq!(reasoning_done["item"]["summary"][0]["type"], "summary_text");
    assert!(
        reasoning_done["item"]["content"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let first_request = captured.recv().await.unwrap();
    let first_body: Value = serde_json::from_slice(&first_request.body).unwrap();
    assert_eq!(first_request.path_and_query, "/v1/responses");
    assert_eq!(
        first_request.authorization.as_deref(),
        Some("Bearer third-party-secret")
    );
    assert!(first_request.chatgpt_account_id.is_none());
    assert_eq!(first_body["model"], "coder");
    assert_eq!(first_body["instructions"], "Use the supplied tools");
    assert_eq!(first_body["tools"].as_array().unwrap().len(), 1);
    assert_eq!(first_body["tools"][0]["name"], "exec_command");
    assert!(first_body.get("previous_response_id").is_none());

    websocket
        .send(Message::text(
            serde_json::json!({
                "type": "response.create",
                "model": "provider-a/coder",
                "previous_response_id": "resp_tool",
                "input": [{
                    "type": "function_call_output",
                    "call_id": "call_pwd",
                    "output": "/tmp"
                }]
            })
            .to_string(),
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), second_request)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        websocket.next().await.unwrap().unwrap(),
        Message::Text(_)
    ));
    let second_body: Value = serde_json::from_slice(&captured.recv().await.unwrap().body).unwrap();
    assert!(second_body.get("previous_response_id").is_none());
    assert_eq!(second_body["input"].as_array().unwrap().len(), 4);
    assert_eq!(second_body["input"][1]["type"], "reasoning");
    assert_eq!(
        second_body["input"][1]["content"][0]["type"],
        "reasoning_text"
    );
    assert!(
        second_body["input"][1]["summary"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(second_body["input"][2]["type"], "function_call");
    assert_eq!(second_body["input"][3]["type"], "function_call_output");
    assert_eq!(second_body["tools"].as_array().unwrap().len(), 1);
    websocket.close(None).await.unwrap();
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn chat_completions_provider_bridges_websocket_tools_and_history() {
    let (upstream, mut captured) = spawn_chat_bridge_upstream().await;
    let configured = chat_provider(upstream);
    let (proxy, shutdown) = spawn_proxy(
        providers(Some(configured), 1_048_576),
        "http://127.0.0.1:9/backend-api/codex".to_owned(),
    )
    .await;
    let (mut websocket, _) = connect_async(ingress_websocket_url(proxy)).await.unwrap();
    websocket.send(Message::text(serde_json::json!({
        "type":"response.create", "model":"provider-a/coder", "generate":false,
        "instructions":"Use tools", "tools":[{"type":"namespace","name":"codex_app","description":"app tools","tools":[{"type":"function","name":"exec_command","parameters":{"type":"object"}}]}]
    }).to_string())).await.unwrap();
    let created: Value =
        serde_json::from_str(websocket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    let warmup_id = created["response"]["id"].as_str().unwrap().to_owned();
    let _ = websocket.next().await.unwrap().unwrap();
    websocket.send(Message::text(serde_json::json!({
        "type":"response.create", "model":"provider-a/coder", "previous_response_id":warmup_id,
        "reasoning":{"effort":"high"}, "input":"pwd"
    }).to_string())).await.unwrap();
    let mut first_events = Vec::new();
    loop {
        let event: Value =
            serde_json::from_str(websocket.next().await.unwrap().unwrap().to_text().unwrap())
                .unwrap();
        let terminal = event["type"] == "response.completed";
        first_events.push(event);
        if terminal {
            break;
        }
    }
    assert!(
        first_events
            .iter()
            .any(|event| event["type"] == "response.reasoning_summary_text.delta")
    );
    assert!(
        first_events
            .iter()
            .any(|event| event["type"] == "response.function_call_arguments.done")
    );
    let first = captured.recv().await.unwrap();
    assert_eq!(first.path_and_query, "/v1/chat/completions");
    assert_eq!(
        first.authorization.as_deref(),
        Some("Bearer third-party-secret")
    );
    let first_body: Value = serde_json::from_slice(&first.body).unwrap();
    assert_eq!(first_body["model"], "coder");
    assert_eq!(first_body["messages"][0]["content"], "Use tools");
    assert_eq!(
        first_body["tools"][0]["function"]["name"],
        "codex_app__exec_command"
    );
    let function_call = first_events
        .iter()
        .find(|event| {
            event["type"] == "response.output_item.done" && event["item"]["type"] == "function_call"
        })
        .unwrap();
    assert_eq!(function_call["item"]["name"], "exec_command");
    assert_eq!(function_call["item"]["namespace"], "codex_app");
    websocket.send(Message::text(serde_json::json!({
        "type":"response.create", "model":"provider-a/coder", "previous_response_id":"chat-tool",
        "input":[{"type":"function_call_output","call_id":"call_pwd","output":"/tmp"}]
    }).to_string())).await.unwrap();
    loop {
        let event: Value =
            serde_json::from_str(websocket.next().await.unwrap().unwrap().to_text().unwrap())
                .unwrap();
        if event["type"] == "response.completed" {
            break;
        }
    }
    let second: Value = serde_json::from_slice(&captured.recv().await.unwrap().body).unwrap();
    assert_eq!(second["messages"][2]["role"], "assistant");
    assert_eq!(second["messages"][2]["reasoning_content"], "inspect");
    assert_eq!(second["messages"][3]["role"], "tool");
    assert_eq!(second["messages"][3]["tool_call_id"], "call_pwd");
    websocket.close(None).await.unwrap();
    shutdown.send(true).unwrap();
}

#[tokio::test]
#[ignore = "requires PROVIDER_X_DEEPSEEK_API_KEY and live DeepSeek access"]
#[allow(clippy::too_many_lines)] // Keep the full two-turn live contract visible in one test.
async fn live_deepseek_v4_pro_chat_completions_tool_contract() {
    let api_key = std::env::var("PROVIDER_X_DEEPSEEK_API_KEY")
        .expect("PROVIDER_X_DEEPSEEK_API_KEY is required");
    let provider_id = ProviderId::new("deepseek").unwrap();
    let model_id = ModelId::new("deepseek-v4-pro").unwrap();
    let configured = ProviderConfig {
        id: provider_id.clone(),
        name: "DeepSeek live contract".to_owned(),
        description: None,
        enabled: true,
        protocol: ProtocolId::OpenaiChatCompletions,
        anthropic_thinking: None,
        endpoints: EndpointConfig {
            http: "https://api.deepseek.com".to_owned(),
            websocket: None,
            models: Some("https://api.deepseek.com/models".to_owned()),
        },
        auth: AuthConfig::Bearer { api_key },
        transports: TransportConfig {
            http_sse: true,
            websocket: false,
        },
    };
    let cache = ModelCacheDocument {
        schema_version: 1,
        providers: BTreeMap::from([(
            provider_id.clone(),
            ProviderModelCache {
                config_fingerprint: configured.routing_fingerprint().unwrap(),
                last_successful_refresh_at: "live".to_owned(),
                source: ProviderModelSource {
                    protocol: ProtocolId::OpenaiChatCompletions,
                    endpoint: "https://api.deepseek.com/models".to_owned(),
                },
                models: vec![ProviderModelSpec {
                    upstream_model_id: model_id.clone(),
                    catalog_model_id: CatalogModelId::for_provider(&provider_id, &model_id),
                    display_name: "DeepSeek V4 Pro".to_owned(),
                    publication_status: ModelPublicationStatus::Ready,
                    context_window: Some(1_000_000),
                    supported_reasoning_levels: vec!["high".to_owned(), "max".to_owned()],
                    supports_parallel_tool_calls: Some(false),
                    supports_search_tool: Some(false),
                    metadata_sources: BTreeMap::new(),
                }],
            },
        )]),
    };
    let (proxy, shutdown) = spawn_proxy_with_cache(
        providers(Some(configured), 1_048_576),
        cache,
        "http://127.0.0.1:9/backend-api/codex".to_owned(),
    )
    .await;
    let (mut websocket, _) = connect_async(ingress_websocket_url(proxy)).await.unwrap();
    websocket
        .send(Message::text(
            serde_json::json!({
                "type":"response.create",
                "model":"deepseek/deepseek-v4-pro",
                "generate":false,
                "instructions":"Call the marker tool when required. After receiving its output, reply with exactly that output.",
                "tools":[
                    {"type":"web_search"},
                    {"type":"tool_search"},
                    {"type":"namespace","name":"contract","description":"Live contract tools","tools":[{"type":"function","name":"get_marker","description":"Returns the required marker","parameters":{"type":"object","properties":{},"required":[],"additionalProperties":false}}]}
                ]
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let created: Value =
        serde_json::from_str(websocket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    let warmup_id = created["response"]["id"].as_str().unwrap().to_owned();
    let _ = websocket.next().await.unwrap().unwrap();
    websocket
        .send(Message::text(
            serde_json::json!({
                "type":"response.create",
                "model":"deepseek/deepseek-v4-pro",
                "previous_response_id":warmup_id,
                "reasoning":{"effort":"high"},
                "tool_choice":"auto",
                "input":"Use the marker tool now."
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let call_id = loop {
        let message = websocket.next().await.unwrap().unwrap();
        let Message::Text(text) = message else {
            panic!("expected text event, got {message:?}");
        };
        let event: Value = serde_json::from_str(text.as_ref()).unwrap();
        if event["type"] == "response.completed" {
            let call = event["response"]["output"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["type"] == "function_call")
                .expect("DeepSeek must return the forced function call");
            assert_eq!(call["name"], "get_marker");
            assert_eq!(call["namespace"], "contract");
            break call["call_id"].as_str().unwrap().to_owned();
        }
    };
    websocket
        .send(Message::text(
            serde_json::json!({
                "type":"response.create",
                "model":"deepseek/deepseek-v4-pro",
                "previous_response_id":"chat-tool",
                "input":[{"type":"function_call_output","call_id":call_id,"output":"DEEPSEEK_CHAT_TOOL_OK"}]
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let mut output = String::new();
    loop {
        let event: Value =
            serde_json::from_str(websocket.next().await.unwrap().unwrap().to_text().unwrap())
                .unwrap();
        if event["type"] == "response.output_text.delta" {
            output.push_str(event["delta"].as_str().unwrap_or_default());
        }
        if event["type"] == "response.completed" {
            break;
        }
    }
    assert_eq!(output.trim(), "DEEPSEEK_CHAT_TOOL_OK");
    websocket.close(None).await.unwrap();
    shutdown.send(true).unwrap();
}

#[tokio::test]
#[ignore = "requires PROVIDER_X_DEEPSEEK_API_KEY and live DeepSeek Anthropic access"]
#[allow(clippy::too_many_lines)] // Keep the full two-turn live contract visible in one test.
async fn live_deepseek_v4_pro_anthropic_messages_tool_contract() {
    let api_key = std::env::var("PROVIDER_X_DEEPSEEK_API_KEY")
        .expect("PROVIDER_X_DEEPSEEK_API_KEY is required");
    let provider_id = ProviderId::new("deepseek").unwrap();
    let model_id = ModelId::new("deepseek-v4-pro").unwrap();
    let configured = ProviderConfig {
        id: provider_id.clone(),
        name: "DeepSeek Anthropic live contract".to_owned(),
        description: None,
        enabled: true,
        protocol: ProtocolId::AnthropicMessages,
        anthropic_thinking: Some(provider_x_core::AnthropicThinkingMode::Enabled),
        endpoints: EndpointConfig {
            http: "https://api.deepseek.com/anthropic".to_owned(),
            websocket: None,
            models: Some("https://api.deepseek.com/models".to_owned()),
        },
        auth: AuthConfig::Bearer { api_key },
        transports: TransportConfig {
            http_sse: true,
            websocket: false,
        },
    };
    let cache = ModelCacheDocument {
        schema_version: 1,
        providers: BTreeMap::from([(
            provider_id.clone(),
            ProviderModelCache {
                config_fingerprint: configured.routing_fingerprint().unwrap(),
                last_successful_refresh_at: "live".to_owned(),
                source: ProviderModelSource {
                    protocol: ProtocolId::AnthropicMessages,
                    endpoint: "https://api.deepseek.com/models".to_owned(),
                },
                models: vec![ProviderModelSpec {
                    upstream_model_id: model_id.clone(),
                    catalog_model_id: CatalogModelId::for_provider(&provider_id, &model_id),
                    display_name: "DeepSeek V4 Pro".to_owned(),
                    publication_status: ModelPublicationStatus::Ready,
                    context_window: Some(1_000_000),
                    supported_reasoning_levels: vec!["high".to_owned(), "max".to_owned()],
                    supports_parallel_tool_calls: Some(false),
                    supports_search_tool: Some(false),
                    metadata_sources: BTreeMap::new(),
                }],
            },
        )]),
    };
    let observer = Arc::new(CollectingObserver::default());
    let contract_observer: Arc<dyn EgressObserver> = observer.clone();
    let (proxy, shutdown) = spawn_proxy_with_cache_and_observer(
        providers(Some(configured), 1_048_576),
        cache,
        "http://127.0.0.1:9/backend-api/codex".to_owned(),
        Some(contract_observer),
    )
    .await;
    let (mut websocket, _) = connect_async(ingress_websocket_url(proxy)).await.unwrap();
    websocket
        .send(Message::text(
            serde_json::json!({
                "type":"response.create",
                "model":"deepseek/deepseek-v4-pro",
                "generate":false,
                "instructions":"Call the marker tool. After receiving its output, reply with exactly that output.",
                "tools":[{"type":"namespace","name":"contract","tools":[{"type":"function","name":"get_marker","description":"Returns the marker","parameters":{"type":"object","properties":{},"required":[],"additionalProperties":false}}]}]
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let created: Value =
        serde_json::from_str(websocket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    let warmup_id = created["response"]["id"].as_str().unwrap().to_owned();
    let _ = websocket.next().await.unwrap().unwrap();
    websocket
        .send(Message::text(
            serde_json::json!({
                "type":"response.create",
                "model":"deepseek/deepseek-v4-pro",
                "previous_response_id":warmup_id,
                "reasoning":{"effort":"high"},
                "tool_choice":{"type":"function","name":"get_marker","namespace":"contract"},
                "input":"Use the marker tool now."
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let mut first_turn_reasoning = false;
    let call_id = loop {
        let message = websocket.next().await.unwrap().unwrap();
        let Message::Text(text) = message else {
            panic!(
                "expected text event, got {message:?}; records={:?}",
                observer.records()
            );
        };
        let event: Value = serde_json::from_str(text.as_ref())
            .unwrap_or_else(|_| panic!("non-JSON text event; records={:?}", observer.records()));
        if event["type"] == "response.reasoning_summary_text.delta" {
            first_turn_reasoning = true;
        }
        if event["type"] == "response.completed" {
            let call = event["response"]["output"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["type"] == "function_call")
                .expect("DeepSeek Anthropic must return the forced function call");
            assert_eq!(call["name"], "get_marker");
            assert_eq!(call["namespace"], "contract");
            break call["call_id"].as_str().unwrap().to_owned();
        }
    };
    assert!(
        first_turn_reasoning,
        "DeepSeek thinking output was not observed"
    );
    websocket
        .send(Message::text(
            serde_json::json!({
                "type":"response.create",
                "model":"deepseek/deepseek-v4-pro",
                "previous_response_id":"anthropic-tool",
                "tool_choice":"auto",
                "input":[{"type":"function_call_output","call_id":call_id,"output":"DEEPSEEK_ANTHROPIC_TOOL_OK"}]
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let mut output = String::new();
    let mut continuation_reasoning = false;
    loop {
        let message = websocket.next().await.unwrap().unwrap();
        let Message::Text(text) = message else {
            panic!(
                "expected text event, got {message:?}; records={:?}",
                observer.records()
            );
        };
        let event: Value = serde_json::from_str(text.as_ref())
            .unwrap_or_else(|_| panic!("non-JSON text event; records={:?}", observer.records()));
        if event["type"] == "response.reasoning_summary_text.delta" {
            continuation_reasoning = true;
        }
        if event["type"] == "response.output_text.delta" {
            output.push_str(event["delta"].as_str().unwrap_or_default());
        }
        if event["type"] == "response.completed" {
            break;
        }
    }
    assert!(
        continuation_reasoning,
        "DeepSeek continuation did not remain in thinking mode"
    );
    assert_eq!(output.trim(), "DEEPSEEK_ANTHROPIC_TOOL_OK");
    websocket.close(None).await.unwrap();
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn closing_bridged_websocket_cancels_inflight_http_response() {
    let (upstream, dropped) = spawn_cancellable_responses_upstream().await;
    let configured = provider(upstream);
    let (proxy, shutdown) = spawn_proxy(
        providers(Some(configured), 1_048_576),
        "http://127.0.0.1:9/backend-api/codex".to_owned(),
    )
    .await;
    let (mut websocket, _) = connect_async(ingress_websocket_url(proxy)).await.unwrap();
    websocket
        .send(Message::text(
            serde_json::json!({
                "type": "response.create",
                "model": "provider-a/coder",
                "input": "keep streaming"
            })
            .to_string(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        websocket.next().await.unwrap().unwrap(),
        Message::Text(_)
    ));
    websocket.close(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), dropped)
        .await
        .expect("closing the downstream WebSocket must drop the HTTP response body")
        .unwrap();
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn bridged_websocket_finishes_at_terminal_event_without_waiting_for_http_eof() {
    let upstream = spawn_terminal_then_stalled_upstream().await;
    let configured = provider(upstream);
    let (proxy, shutdown) = spawn_proxy(
        providers(Some(configured), 1_048_576),
        "http://127.0.0.1:9/backend-api/codex".to_owned(),
    )
    .await;
    let request = ingress_websocket_url(proxy).into_client_request().unwrap();
    let (mut websocket, _) = connect_async(request).await.unwrap();
    websocket
        .send(Message::text(
            serde_json::json!({
                "type": "response.create",
                "model": "provider-a/coder",
                "input": "hello"
            })
            .to_string(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_millis(250), websocket.next())
            .await
            .expect("terminal event must not wait for HTTP EOF")
            .unwrap()
            .unwrap(),
        Message::Text(_)
    ));
    websocket.close(None).await.unwrap();
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn websocket_waiting_for_first_message_honors_idle_timeout() {
    let mut configuration = providers(None, 1_048_576);
    configuration.timeouts.websocket_idle_ms = 50;
    let (proxy, shutdown) = spawn_proxy(
        configuration,
        "http://127.0.0.1:9/backend-api/codex".to_owned(),
    )
    .await;
    let request = ingress_websocket_url(proxy).into_client_request().unwrap();
    let (mut websocket, _) = connect_async(request).await.unwrap();

    let error = tokio::time::timeout(Duration::from_millis(500), websocket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let Message::Text(error) = error else {
        panic!("expected an explicit idle_timeout event");
    };
    let error: Value = serde_json::from_str(error.as_ref()).unwrap();
    assert_eq!(error["error"]["code"], "idle_timeout");
    assert!(matches!(
        websocket.next().await.unwrap().unwrap(),
        Message::Close(_)
    ));
    shutdown.send(true).unwrap();
}

#[tokio::test]
async fn shutdown_closes_an_idle_completed_websocket_immediately() {
    let (upstream, mut captured, upstream_closed) = spawn_websocket_upstream().await;
    let mut configuration = providers(None, 1_048_576);
    configuration.timeouts.shutdown_grace_ms = 2_000;
    let (proxy, shutdown, server_task) = spawn_proxy_with_task(
        configuration,
        format!("http://{upstream}/backend-api/codex"),
    )
    .await;
    let request = ingress_websocket_url(proxy).into_client_request().unwrap();
    let (mut websocket, _) = connect_async(request).await.unwrap();
    websocket
        .send(Message::text(
            serde_json::json!({
                "type": "response.create",
                "model": "gpt-5.6",
                "input": "hello"
            })
            .to_string(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        websocket.next().await.unwrap().unwrap(),
        Message::Text(_)
    ));
    captured.recv().await.unwrap();

    let started = std::time::Instant::now();
    shutdown.send(true).unwrap();
    let close = tokio::time::timeout(Duration::from_millis(500), websocket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let Message::Close(Some(frame)) = close else {
        panic!("expected a service restart close frame");
    };
    assert_eq!(frame.code, CloseCode::Restart);
    assert!(started.elapsed() < Duration::from_millis(500));
    tokio::time::timeout(Duration::from_secs(2), upstream_closed)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_millis(500), server_task)
        .await
        .expect("server task must not wait for the full grace period")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn shutdown_waits_for_an_active_websocket_to_reach_terminal_then_closes() {
    let (upstream, upstream_closed) =
        spawn_delayed_websocket_upstream(Duration::from_millis(150)).await;
    let mut configuration = providers(None, 1_048_576);
    configuration.timeouts.shutdown_grace_ms = 2_000;
    let (proxy, shutdown, server_task) = spawn_proxy_with_task(
        configuration,
        format!("http://{upstream}/backend-api/codex"),
    )
    .await;
    let request = ingress_websocket_url(proxy).into_client_request().unwrap();
    let (mut websocket, _) = connect_async(request).await.unwrap();
    websocket
        .send(Message::text(
            serde_json::json!({
                "type": "response.create",
                "model": "gpt-5.6",
                "input": "hello"
            })
            .to_string(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        websocket.next().await.unwrap().unwrap(),
        Message::Text(_)
    ));

    shutdown.send(true).unwrap();
    let terminal = tokio::time::timeout(Duration::from_secs(1), websocket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let Message::Text(terminal) = terminal else {
        panic!("active response was interrupted before terminal")
    };
    assert!(is_terminal_ws_event(terminal.as_ref()));
    let close = websocket.next().await.unwrap().unwrap();
    let Message::Close(Some(frame)) = close else {
        panic!("expected close after terminal")
    };
    assert_eq!(frame.code, CloseCode::Restart);
    tokio::time::timeout(Duration::from_secs(1), upstream_closed)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), server_task)
        .await
        .expect("server task must exit after the terminal event")
        .unwrap()
        .unwrap();
}
