use std::{
    collections::BTreeMap,
    convert::Infallible,
    env,
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, StatusCode, body::Incoming, service::service_fn};
use hyper_util::rt::TokioIo;
use provider_x_core::{
    AuthConfig, CatalogModelId, CodexConfig, EndpointConfig, ListenerConfig, ModelCacheDocument,
    ModelId, ModelPublicationStatus, ProtocolId, ProviderConfig, ProviderId, ProviderModelCache,
    ProviderModelSource, ProviderModelSpec, ProvidersDocument, TimeoutConfig, TransportConfig,
};
use provider_x_egress::{
    EgressEvent, EgressObserver, EgressServer, EgressState, IngressCapability,
};
use serde::Serialize;
use thiserror::Error;
use tokio::sync::watch;
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        handshake::server::{Request as WsHandshakeRequest, Response as WsHandshakeResponse},
        protocol::{CloseFrame, Message, frame::coding::CloseCode},
    },
};

const DEFAULT_OFFICIAL_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const PROBE_CAPABILITY: &str = "c03dec03dec03dec03dec03dec03dec03dec03dec03dec03dec03dec03dec03d";

#[derive(Debug, Error)]
enum ProbeError {
    #[error("{0}")]
    Usage(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid egress configuration: {0}")]
    EgressBuild(#[from] provider_x_egress::EgressBuildError),

    #[error("invalid contract fixture: {0}")]
    Core(#[from] provider_x_core::CoreError),

    #[error("evidence serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("egress task failed: {0}")]
    Join(#[from] tokio::task::JoinError),

    #[error("contract observer failed: {0}")]
    Observer(String),
}

#[derive(Clone, Debug)]
struct ProbeArgs {
    case_name: String,
    listen: SocketAddr,
    official_base_url: String,
    evidence_path: PathBuf,
    codex_version: String,
    provider_x_commit: String,
    macos_version: String,
    transport: Option<String>,
    scenario: Option<String>,
}

impl ProbeArgs {
    fn parse() -> Result<Self, ProbeError> {
        let mut case_name = None;
        let mut listen: Option<SocketAddr> = None;
        let mut official_base_url = DEFAULT_OFFICIAL_BASE_URL.to_owned();
        let mut evidence_path = None;
        let mut codex_version = "unknown".to_owned();
        let mut provider_x_commit = "unknown".to_owned();
        let mut macos_version = "unknown".to_owned();
        let mut transport = None;
        let mut scenario = None;
        let mut arguments = env::args().skip(1);
        while let Some(argument) = arguments.next() {
            let value = match argument.as_str() {
                "--help" | "-h" => return Err(ProbeError::Usage(usage())),
                "--case"
                | "--listen"
                | "--official-base-url"
                | "--evidence"
                | "--codex-version"
                | "--provider-x-commit"
                | "--macos-version"
                | "--transport"
                | "--scenario" => arguments
                    .next()
                    .ok_or_else(|| ProbeError::Usage(format!("missing value for {argument}")))?,
                _ => return Err(ProbeError::Usage(format!("unknown argument {argument}"))),
            };
            match argument.as_str() {
                "--case" => case_name = Some(value),
                "--listen" => {
                    listen = Some(value.parse().map_err(|_| {
                        ProbeError::Usage("--listen must be a socket address".to_owned())
                    })?);
                }
                "--official-base-url" => official_base_url = value,
                "--evidence" => evidence_path = Some(PathBuf::from(value)),
                "--codex-version" => codex_version = value,
                "--provider-x-commit" => provider_x_commit = value,
                "--macos-version" => macos_version = value,
                "--transport" => transport = Some(value),
                "--scenario" => scenario = Some(value),
                _ => {
                    return Err(ProbeError::Usage(
                        "internal argument parser mismatch".to_owned(),
                    ));
                }
            }
        }

        let case_name = case_name.ok_or_else(|| ProbeError::Usage(usage()))?;
        if !matches!(
            case_name.as_str(),
            "C01" | "C02" | "C03" | "C04" | "C05" | "C06" | "C07"
        ) {
            return Err(ProbeError::Usage(
                "the probe supports only --case C01, C02, C03, C04, C05, C06, or C07".to_owned(),
            ));
        }
        if case_name == "C03" && !matches!(transport.as_deref(), Some("http" | "websocket")) {
            return Err(ProbeError::Usage(
                "C03 requires --transport http or websocket".to_owned(),
            ));
        }
        if case_name != "C03" && transport.is_some() {
            return Err(ProbeError::Usage(
                "--transport is only valid for C03".to_owned(),
            ));
        }
        if case_name == "C04" && !matches!(scenario.as_deref(), None | Some("route" | "cancel")) {
            return Err(ProbeError::Usage(
                "C04 --scenario must be route or cancel".to_owned(),
            ));
        }
        if case_name != "C04" && scenario.is_some() {
            return Err(ProbeError::Usage(
                "--scenario is only valid for C04".to_owned(),
            ));
        }
        let listen = listen.ok_or_else(|| ProbeError::Usage(usage()))?;
        if listen.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) || listen.port() == 0 {
            return Err(ProbeError::Usage(
                "--listen must use 127.0.0.1 and a non-zero port".to_owned(),
            ));
        }
        let evidence_path = evidence_path.ok_or_else(|| ProbeError::Usage(usage()))?;
        Ok(Self {
            case_name,
            listen,
            official_base_url,
            evidence_path,
            codex_version,
            provider_x_commit,
            macos_version,
            transport,
            scenario,
        })
    }
}

fn usage() -> String {
    "usage: provider-x-contract-probe --case <C01|C02|C03|C04|C05|C06|C07> --listen 127.0.0.1:43119 \
--evidence <new-path> [--official-base-url <url>] [--codex-version <version>] \
[--provider-x-commit <commit>] [--macos-version <version>] \
[--transport <http|websocket>] [--scenario <route|cancel>]"
        .to_owned()
}

#[derive(Serialize)]
struct EvidenceEnvelope<'a, T> {
    schema_version: u32,
    case: &'a str,
    observed_at_unix_ms: u128,
    codex_version: &'a str,
    provider_x_commit: &'a str,
    macos_version: &'a str,
    #[serde(flatten)]
    event: &'a T,
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum ProbeLifecycleEvent<'a> {
    MockUpstreamClosed {
        transport: &'a str,
        provider_id: &'a str,
        pending_response: bool,
        close_kind: &'a str,
    },
}

struct EvidenceObserver {
    case_name: String,
    codex_version: String,
    provider_x_commit: String,
    macos_version: String,
    writer: Mutex<BufWriter<File>>,
    event_count: AtomicUsize,
    first_error: Mutex<Option<String>>,
}

impl EvidenceObserver {
    fn create(args: &ProbeArgs) -> Result<Self, ProbeError> {
        create_private_parent(&args.evidence_path)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&args.evidence_path)?;
        Ok(Self {
            case_name: args.case_name.clone(),
            codex_version: args.codex_version.clone(),
            provider_x_commit: args.provider_x_commit.clone(),
            macos_version: args.macos_version.clone(),
            writer: Mutex::new(BufWriter::new(file)),
            event_count: AtomicUsize::new(0),
            first_error: Mutex::new(None),
        })
    }

    fn event_count(&self) -> usize {
        self.event_count.load(Ordering::Relaxed)
    }

    fn finish(&self) -> Result<(), ProbeError> {
        if let Some(error) = self
            .first_error
            .lock()
            .map_err(|_| ProbeError::Observer("error state lock was poisoned".to_owned()))?
            .clone()
        {
            return Err(ProbeError::Observer(error));
        }
        self.writer
            .lock()
            .map_err(|_| ProbeError::Observer("evidence writer lock was poisoned".to_owned()))?
            .flush()?;
        Ok(())
    }

    fn remember_error(&self, error: &impl ToString) {
        if let Ok(mut first_error) = self.first_error.lock()
            && first_error.is_none()
        {
            *first_error = Some(error.to_string());
        }
    }

    fn record_serializable<T: Serialize>(&self, event: &T) {
        let envelope = EvidenceEnvelope {
            schema_version: 1,
            case: &self.case_name,
            observed_at_unix_ms: unix_time_ms(),
            codex_version: &self.codex_version,
            provider_x_commit: &self.provider_x_commit,
            macos_version: &self.macos_version,
            event,
        };
        let result = self
            .writer
            .lock()
            .map_err(|_| "evidence writer lock was poisoned".to_owned())
            .and_then(|mut writer| {
                serde_json::to_writer(&mut *writer, &envelope)
                    .map_err(|error| error.to_string())?;
                writer.write_all(b"\n").map_err(|error| error.to_string())?;
                writer.flush().map_err(|error| error.to_string())
            });
        if let Err(error) = result {
            self.remember_error(&error);
        } else {
            self.event_count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl EgressObserver for EvidenceObserver {
    fn record(&self, event: EgressEvent) {
        self.record_serializable(&event);
    }
}

fn create_private_parent(path: &Path) -> Result<(), ProbeError> {
    let Some(parent) = path.parent() else {
        return Err(ProbeError::Usage(
            "--evidence must include a parent directory".to_owned(),
        ));
    };
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn unix_time_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn probe_config(args: &ProbeArgs, mock_provider: Option<SocketAddr>) -> ProvidersDocument {
    let providers = mock_provider.map_or_else(Vec::new, |address| {
        let websocket_enabled = matches!(args.case_name.as_str(), "C04" | "C06")
            || args.transport.as_deref() == Some("websocket");
        let websocket = websocket_enabled.then(|| format!("ws://{address}/v1/responses"));
        let provider_ids: &[&str] = if args.case_name == "C06" {
            &["provider-a", "provider-b"]
        } else {
            &["provider-a"]
        };
        provider_ids
            .iter()
            .map(|provider_id| ProviderConfig {
                id: ProviderId::new(*provider_id).expect("fixture Provider ID is valid"),
                name: format!("M0 {provider_id}"),
                description: Some("strict local contract fixture".to_owned()),
                enabled: true,
                protocol: ProtocolId::OpenaiResponses,
                anthropic_thinking: None,
                endpoints: EndpointConfig {
                    http: format!("http://{address}/v1"),
                    websocket: websocket.clone(),
                    models: None,
                },
                auth: AuthConfig::Bearer {
                    api_key: "m0-contract-key".to_owned(),
                },
                transports: TransportConfig {
                    http_sse: true,
                    websocket: websocket_enabled,
                },
            })
            .collect()
    });
    ProvidersDocument {
        schema_version: 1,
        listener: ListenerConfig {
            host: args.listen.ip().to_string(),
            port: args.listen.port(),
            request_body_limit_bytes: 32 * 1024 * 1024,
            max_connections: 32,
        },
        timeouts: TimeoutConfig {
            request_body_ms: 30_000,
            connect_ms: 10_000,
            response_headers_ms: 30_000,
            stream_idle_ms: 300_000,
            websocket_idle_ms: 300_000,
            shutdown_grace_ms: 10_000,
        },
        codex: CodexConfig {
            manage_user_config: false,
        },
        providers,
    }
}

fn probe_cache(config: &ProvidersDocument) -> Result<ModelCacheDocument, ProbeError> {
    if config.providers.is_empty() {
        return Ok(ModelCacheDocument {
            schema_version: 1,
            providers: BTreeMap::new(),
        });
    }
    let mut providers = BTreeMap::new();
    for provider in &config.providers {
        let upstream_model_id = ModelId::new("coder")?;
        providers.insert(
            provider.id.clone(),
            ProviderModelCache {
                config_fingerprint: provider.routing_fingerprint()?,
                last_successful_refresh_at: "2026-08-12T00:00:00Z".to_owned(),
                source: ProviderModelSource {
                    protocol: ProtocolId::OpenaiResponses,
                    endpoint: format!("{}/models", provider.endpoints.http),
                },
                models: vec![ProviderModelSpec {
                    catalog_model_id: CatalogModelId::for_provider(
                        &provider.id,
                        &upstream_model_id,
                    ),
                    upstream_model_id,
                    display_name: "Coder".to_owned(),
                    publication_status: ModelPublicationStatus::Ready,
                    context_window: Some(128_000),
                    supported_reasoning_levels: vec!["low".to_owned()],
                    supports_parallel_tool_calls: Some(true),
                    supports_search_tool: Some(false),
                    metadata_sources: BTreeMap::new(),
                }],
            },
        );
    }
    let cache = ModelCacheDocument {
        schema_version: 1,
        providers,
    };
    Ok(cache)
}

async fn spawn_strict_http_provider()
-> Result<(SocketAddr, watch::Sender<bool>, tokio::task::JoinHandle<()>), ProbeError> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                    continue;
                }
                accepted = listener.accept() => accepted,
            };
            let Ok((stream, _)) = accepted else {
                break;
            };
            tokio::spawn(async move {
                let service = service_fn(strict_http_provider_response);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    Ok((address, shutdown_tx, task))
}

async fn strict_http_provider_response(
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let valid_path =
        request.method() == hyper::Method::POST && request.uri().path() == "/v1/responses";
    let valid_auth = request
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .is_some_and(|value| value == "Bearer m0-contract-key");
    let official_credentials_absent = !request.headers().contains_key("chatgpt-account-id")
        && !request.headers().contains_key("x-openai-attestation");
    let encoding = request
        .headers()
        .get(hyper::header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = request
        .into_body()
        .collect()
        .await
        .map_or_else(|_| Bytes::new(), http_body_util::Collected::to_bytes);
    let decoded = match encoding.as_deref() {
        None | Some("identity") => Some(body.to_vec()),
        Some("zstd") => zstd::stream::decode_all(std::io::Cursor::new(body)).ok(),
        Some(_) => None,
    };
    let valid_model = decoded
        .as_deref()
        .and_then(|body| serde_json::from_slice::<serde_json::Value>(body).ok())
        .and_then(|body| {
            body.get("model")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some("coder");

    if !(valid_path && valid_auth && official_credentials_absent && valid_model) {
        return Ok(Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Full::new(Bytes::from_static(b"contract mismatch")))
            .expect("static mock response is valid"));
    }

    let events = [
        serde_json::json!({"type":"response.created","response":{"id":"resp-c03-http"}}),
        serde_json::json!({
            "type":"response.output_item.done",
            "item":{
                "type":"message",
                "role":"assistant",
                "id":"msg-c03-http",
                "content":[{"type":"output_text","text":"C03_HTTP_OK"}]
            }
        }),
        serde_json::json!({
            "type":"response.completed",
            "response":{
                "id":"resp-c03-http",
                "usage":{
                    "input_tokens":1,
                    "input_tokens_details":null,
                    "output_tokens":1,
                    "output_tokens_details":null,
                    "total_tokens":2
                }
            }
        }),
    ];
    let mut sse = String::new();
    for event in events {
        let kind = event["type"].as_str().unwrap_or_default();
        write!(sse, "event: {kind}\ndata: {event}\n\n")
            .expect("writing to an in-memory string cannot fail");
    }
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "text/event-stream")
        .body(Full::new(Bytes::from(sse)))
        .expect("static mock response is valid"))
}

#[allow(clippy::result_large_err)]
async fn spawn_strict_websocket_provider(
    response_text: &'static str,
    response_prefix: &'static str,
    hold_generated_response: bool,
    observer: Arc<EvidenceObserver>,
) -> Result<(SocketAddr, watch::Sender<bool>, tokio::task::JoinHandle<()>), ProbeError> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                    continue;
                }
                accepted = listener.accept() => accepted,
            };
            let Ok((stream, _)) = accepted else {
                break;
            };
            tokio::spawn(strict_websocket_provider_session(
                stream,
                response_text,
                response_prefix,
                hold_generated_response,
                Arc::clone(&observer),
            ));
        }
    });
    Ok((address, shutdown_tx, task))
}

#[allow(clippy::result_large_err)]
async fn strict_websocket_provider_session(
    stream: tokio::net::TcpStream,
    response_text: &'static str,
    response_prefix: &'static str,
    hold_generated_response: bool,
    observer: Arc<EvidenceObserver>,
) {
    let handshake_valid = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handshake_valid_in_callback = Arc::clone(&handshake_valid);
    let accepted = accept_hdr_async(
        stream,
        move |request: &WsHandshakeRequest, response: WsHandshakeResponse| {
            let valid = request.uri().path() == "/v1/responses"
                && request
                    .headers()
                    .get(hyper::header::AUTHORIZATION)
                    .is_some_and(|value| value == "Bearer m0-contract-key")
                && !request.headers().contains_key("chatgpt-account-id")
                && !request.headers().contains_key("x-openai-attestation");
            handshake_valid_in_callback.store(valid, Ordering::Release);
            Ok(response)
        },
    )
    .await;
    let Ok(mut websocket) = accepted else {
        return;
    };
    if !handshake_valid.load(Ordering::Acquire) {
        let _ = websocket
            .close(Some(CloseFrame {
                code: CloseCode::Policy,
                reason: "contract mismatch".into(),
            }))
            .await;
        return;
    }

    let mut sequence = 0_u64;
    let mut pending_response = false;
    while let Some(message) = websocket.next().await {
        match message {
            Ok(Message::Text(text)) => {
                let Ok(create) = serde_json::from_str::<serde_json::Value>(&text) else {
                    return;
                };
                if create["type"] != "response.create" || create["model"] != "coder" {
                    return;
                }
                sequence += 1;
                let output = (create["generate"] != false).then_some(response_text);
                let events = if hold_generated_response && output.is_some() {
                    pending_response = true;
                    strict_response_events(response_prefix, sequence, None)
                        .into_iter()
                        .take(1)
                        .collect()
                } else {
                    strict_response_events(response_prefix, sequence, output)
                };
                for event in events {
                    if websocket
                        .send(Message::text(event.to_string()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            Ok(Message::Ping(payload)) => {
                if websocket.send(Message::Pong(payload)).await.is_err() {
                    return;
                }
            }
            Ok(Message::Close(_)) => {
                record_mock_close(&observer, pending_response, "close_frame");
                return;
            }
            Err(_) => {
                record_mock_close(&observer, pending_response, "transport_error");
                return;
            }
            Ok(Message::Binary(_) | Message::Pong(_) | Message::Frame(_)) => {}
        }
    }
    record_mock_close(&observer, pending_response, "transport_eof");
}

fn record_mock_close(
    observer: &EvidenceObserver,
    pending_response: bool,
    close_kind: &'static str,
) {
    if pending_response {
        observer.record_serializable(&ProbeLifecycleEvent::MockUpstreamClosed {
            transport: "websocket",
            provider_id: "provider-a",
            pending_response,
            close_kind,
        });
    }
}

fn strict_response_events(
    response_prefix: &str,
    sequence: u64,
    output: Option<&str>,
) -> Vec<serde_json::Value> {
    let response_id = format!("resp-{response_prefix}-{sequence}");
    let mut events =
        vec![serde_json::json!({"type":"response.created","response":{"id":response_id}})];
    if let Some(text) = output {
        events.push(serde_json::json!({
            "type":"response.output_item.done",
            "item":{
                "type":"message",
                "role":"assistant",
                "id":format!("msg-{response_prefix}-{sequence}"),
                "content":[{"type":"output_text","text":text}]
            }
        }));
    }
    events.push(serde_json::json!({
        "type":"response.completed",
        "response":{
            "id":response_id,
            "usage":{
                "input_tokens":1,
                "input_tokens_details":null,
                "output_tokens":1,
                "output_tokens_details":null,
                "total_tokens":2
            }
        }
    }));
    events
}

async fn shutdown_signal() -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }

    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}

#[tokio::main]
async fn main() -> Result<(), ProbeError> {
    let args = ProbeArgs::parse()?;
    let observer = Arc::new(EvidenceObserver::create(&args)?);
    let mock_provider = match (args.case_name.as_str(), args.transport.as_deref()) {
        ("C03", Some("http")) | ("C07", None) => Some(spawn_strict_http_provider().await?),
        ("C03", Some("websocket")) => Some(
            spawn_strict_websocket_provider("C03_WS_OK", "c03-ws", false, Arc::clone(&observer))
                .await?,
        ),
        ("C04", None) => Some(
            spawn_strict_websocket_provider(
                "C04_CHILD_OK",
                "c04-child",
                args.scenario.as_deref() == Some("cancel"),
                Arc::clone(&observer),
            )
            .await?,
        ),
        ("C06", None) => Some(
            spawn_strict_websocket_provider(
                "C06_THIRD_PARTY_OK",
                "c06-third-party",
                false,
                Arc::clone(&observer),
            )
            .await?,
        ),
        _ => None,
    };
    let config = probe_config(&args, mock_provider.as_ref().map(|provider| provider.0));
    let cache = probe_cache(&config)?;
    let state = EgressState::new(
        &config,
        &cache,
        args.official_base_url.clone(),
        IngressCapability::from_hex(PROBE_CAPABILITY)?,
    )?
    .with_observer(observer.clone());
    let state = if args.case_name == "C01" || args.transport.as_deref() == Some("http") {
        state.with_websocket_fallback_on_upgrade()
    } else {
        state
    };
    let server = EgressServer::bind(args.listen, Arc::new(state)).await?;
    let address = server.local_addr()?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut server_task = tokio::spawn(server.run(shutdown_rx));

    println!(
        "provider-x M0 {} probe listening on {}; openai_base_url=http://{}/{}/v1; evidence={}",
        args.case_name,
        address,
        address,
        PROBE_CAPABILITY,
        args.evidence_path.display()
    );
    tokio::select! {
        signal = shutdown_signal() => signal?,
        result = &mut server_task => {
            result??;
            return Err(ProbeError::Observer(
                "egress server stopped before the shutdown signal".to_owned(),
            ));
        }
    }
    let _ = shutdown_tx.send(true);
    tokio::time::timeout(Duration::from_secs(15), server_task)
        .await
        .map_err(|_| ProbeError::Observer("egress shutdown timed out".to_owned()))???;
    if let Some((_address, mock_shutdown, mock_task)) = mock_provider {
        let _ = mock_shutdown.send(true);
        tokio::time::timeout(Duration::from_secs(5), mock_task)
            .await
            .map_err(|_| ProbeError::Observer("mock Provider shutdown timed out".to_owned()))??;
    }
    observer.finish()?;
    println!(
        "provider-x M0 {} probe stopped; redacted_events={}",
        args.case_name,
        observer.event_count()
    );
    Ok(())
}
