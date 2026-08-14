use std::time::Duration;

use futures_util::future::poll_fn;
use hyper::Uri;
use hyper_http_proxy::{Intercept, Proxy, ProxyConnector, ProxyStream};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder, MaybeHttpsStream};
use hyper_util::{client::legacy::connect::HttpConnector, rt::TokioIo};
use provider_x_core::ProxyEnvironment;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    WebSocketStream, client_async_with_config,
    tungstenite::{
        handshake::client::{Request as WebSocketRequest, Response as WebSocketResponse},
        protocol::WebSocketConfig,
    },
};
use tower_service::Service;

pub type NetworkConnector = ProxyConnector<HttpsConnector<HttpConnector>>;
pub type NetworkWebSocket =
    WebSocketStream<TokioIo<ProxyStream<MaybeHttpsStream<TokioIo<TcpStream>>>>>;

#[derive(Debug, Error)]
pub enum ProxyConfigurationError {
    #[error("invalid HTTP proxy configuration")]
    InvalidUri,

    #[error("failed to initialize proxy support")]
    Initialization,
}

#[derive(Debug, Error)]
pub enum WebSocketConnectionError {
    #[error("invalid WebSocket endpoint")]
    InvalidEndpoint,

    #[error("failed to establish WebSocket transport")]
    Transport,

    #[error("upstream WebSocket handshake failed")]
    Handshake,
}

/// Builds the HTTP connector used by catalog and HTTP/SSE egress traffic.
///
/// # Errors
///
/// Returns an error when proxy configuration or TLS initialization fails.
pub fn build_http_connector(
    environment: &ProxyEnvironment,
    connect_timeout: Duration,
) -> Result<NetworkConnector, ProxyConfigurationError> {
    build_connector(environment, connect_timeout, true)
}

/// Builds the HTTP/1.1-only connector used to establish WebSocket transports through the same
/// proxy and `NO_PROXY` policy as HTTP traffic.
///
/// # Errors
///
/// Returns an error when proxy configuration or TLS initialization fails.
pub fn build_websocket_connector(
    environment: &ProxyEnvironment,
    connect_timeout: Duration,
) -> Result<NetworkConnector, ProxyConfigurationError> {
    build_connector(environment, connect_timeout, false)
}

/// Establishes a WebSocket over a connector that has already applied direct/proxy routing and
/// target TLS. The WebSocket request retains its original `ws`/`wss` URI for the handshake.
///
/// # Errors
///
/// Returns an error for an invalid endpoint, transport failure, or rejected handshake.
pub async fn connect_websocket(
    mut connector: NetworkConnector,
    request: WebSocketRequest,
    config: Option<WebSocketConfig>,
) -> Result<(NetworkWebSocket, WebSocketResponse), WebSocketConnectionError> {
    let connector_uri = websocket_connector_uri(request.uri())?;
    poll_fn(|context| connector.poll_ready(context))
        .await
        .map_err(|_| WebSocketConnectionError::Transport)?;
    let stream = connector
        .call(connector_uri)
        .await
        .map_err(|_| WebSocketConnectionError::Transport)?;
    client_async_with_config(request, TokioIo::new(stream), config)
        .await
        .map_err(|_| WebSocketConnectionError::Handshake)
}

fn build_connector(
    environment: &ProxyEnvironment,
    connect_timeout: Duration,
    enable_http2: bool,
) -> Result<NetworkConnector, ProxyConfigurationError> {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_connect_timeout(Some(connect_timeout));
    let direct = if enable_http2 {
        HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(http)
    } else {
        HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .wrap_connector(http)
    };
    let mut connector =
        ProxyConnector::new(direct).map_err(|_| ProxyConfigurationError::Initialization)?;
    connector.extend_proxies(build_http_proxies(environment)?);
    Ok(connector)
}

fn websocket_connector_uri(uri: &Uri) -> Result<Uri, WebSocketConnectionError> {
    let scheme = match uri.scheme_str() {
        Some("ws") => "http",
        Some("wss") => "https",
        _ => return Err(WebSocketConnectionError::InvalidEndpoint),
    };
    Uri::builder()
        .scheme(scheme)
        .authority(
            uri.authority()
                .cloned()
                .ok_or(WebSocketConnectionError::InvalidEndpoint)?,
        )
        .path_and_query(
            uri.path_and_query()
                .cloned()
                .ok_or(WebSocketConnectionError::InvalidEndpoint)?,
        )
        .build()
        .map_err(|_| WebSocketConnectionError::InvalidEndpoint)
}

/// Builds scheme-specific HTTP proxy definitions from one immutable environment snapshot.
///
/// # Errors
///
/// Returns an error when a configured proxy URL is not an absolute HTTP(S) URI.
pub fn build_http_proxies(
    environment: &ProxyEnvironment,
) -> Result<Vec<Proxy>, ProxyConfigurationError> {
    ["http", "https"]
        .into_iter()
        .filter_map(|scheme| {
            environment
                .proxy_url(scheme)
                .map(|proxy_url| proxy_for_scheme(environment, scheme, proxy_url))
        })
        .collect()
}

fn proxy_for_scheme(
    environment: &ProxyEnvironment,
    expected_scheme: &'static str,
    proxy_uri: &str,
) -> Result<Proxy, ProxyConfigurationError> {
    let uri = parse_proxy_uri(proxy_uri)?;
    let environment = environment.clone();
    let intercept = Intercept::from(
        move |scheme: Option<&str>, host: Option<&str>, port: Option<u16>| {
            scheme == Some(expected_scheme)
                && host.is_some_and(|host| environment.should_proxy(expected_scheme, host, port))
        },
    );
    let mut proxy = Proxy::new(intercept, uri);
    // CONNECT keeps proxy authorization on the proxy hop for both HTTP and HTTPS targets.
    proxy.force_connect();
    Ok(proxy)
}

fn parse_proxy_uri(value: &str) -> Result<Uri, ProxyConfigurationError> {
    let uri = value
        .parse::<Uri>()
        .map_err(|_| ProxyConfigurationError::InvalidUri)?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.host().is_none() {
        return Err(ProxyConfigurationError::InvalidUri);
    }
    Ok(uri)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt, future::poll_fn};
    use hyper::Uri;
    use provider_x_core::ProxyEnvironment;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };
    use tokio_tungstenite::{
        accept_async,
        tungstenite::{client::IntoClientRequest, protocol::Message},
    };
    use tower_service::Service;

    use super::{
        build_http_connector, build_http_proxies, build_websocket_connector, connect_websocket,
        parse_proxy_uri, websocket_connector_uri,
    };

    #[test]
    fn rejects_invalid_or_unsupported_proxy_uri() {
        assert!(parse_proxy_uri("not a uri").is_err());
        assert!(parse_proxy_uri("socks5://127.0.0.1:1080").is_err());
        assert!(parse_proxy_uri("http://127.0.0.1:8080").is_ok());
    }

    #[test]
    fn builds_scheme_specific_proxies_from_shared_policy() {
        let environment = ProxyEnvironment::from_values(
            Some("http://127.0.0.1:8080"),
            Some("http://127.0.0.1:8080"),
            "localhost",
        );
        let proxies = build_http_proxies(&environment).unwrap();
        assert_eq!(proxies.len(), 2);
        assert!(
            proxies[0]
                .intercept()
                .matches(&"http://example.com".parse::<hyper::Uri>().unwrap())
        );
        assert!(
            !proxies[0]
                .intercept()
                .matches(&"https://example.com".parse::<hyper::Uri>().unwrap())
        );
        assert!(
            !proxies[1]
                .intercept()
                .matches(&"https://localhost".parse::<hyper::Uri>().unwrap())
        );
    }

    #[test]
    fn websocket_uris_use_the_corresponding_http_proxy_policy_scheme() {
        assert_eq!(
            websocket_connector_uri(&"ws://example.com/socket".parse().unwrap())
                .unwrap()
                .scheme_str(),
            Some("http")
        );
        assert_eq!(
            websocket_connector_uri(&"wss://example.com/socket".parse().unwrap())
                .unwrap()
                .scheme_str(),
            Some("https")
        );
    }

    #[test]
    fn http_and_websocket_connectors_share_proxy_selection() {
        let environment = ProxyEnvironment::from_values(
            Some("http://127.0.0.1:8080"),
            Some("http://127.0.0.1:8080"),
            "direct.example",
        );
        let http = build_http_connector(&environment, Duration::from_secs(1)).unwrap();
        let websocket = build_websocket_connector(&environment, Duration::from_secs(1)).unwrap();

        for connector in [&http, &websocket] {
            assert!(
                connector.proxies()[0]
                    .intercept()
                    .matches(&"http://proxied.example".parse::<Uri>().unwrap())
            );
            assert!(
                !connector.proxies()[0]
                    .intercept()
                    .matches(&"http://direct.example".parse::<Uri>().unwrap())
            );
            assert!(
                connector.proxies()[1]
                    .intercept()
                    .matches(&"https://proxied.example".parse::<Uri>().unwrap())
            );
            assert!(
                !connector.proxies()[1]
                    .intercept()
                    .matches(&"https://direct.example".parse::<Uri>().unwrap())
            );
        }
    }

    #[tokio::test]
    async fn direct_https_connector_reaches_target_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let environment = ProxyEnvironment::from_values(None, None, "");
        let mut connector = build_http_connector(&environment, Duration::from_secs(1)).unwrap();
        let uri = format!("https://{address}/").parse::<Uri>().unwrap();

        let connection = tokio::spawn(async move {
            poll_fn(|context| connector.poll_ready(context))
                .await
                .unwrap();
            connector.call(uri).await
        });
        let (stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("HTTPS connector did not reach the target socket")
            .unwrap();
        drop(stream);

        let result = tokio::time::timeout(Duration::from_secs(2), connection)
            .await
            .unwrap()
            .unwrap();
        assert!(
            result.is_err(),
            "a plaintext test socket must not complete the TLS handshake"
        );
    }

    #[tokio::test]
    async fn websocket_connection_traverses_selected_http_connect_proxy() {
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream = tokio::spawn(async move {
            let (stream, _) = upstream_listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            let message = websocket.next().await.unwrap().unwrap();
            websocket.send(message).await.unwrap();
        });

        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy = tokio::spawn(async move {
            let (mut downstream, _) = proxy_listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let length = downstream.read(&mut buffer).await.unwrap();
                assert_ne!(length, 0, "proxy client closed before CONNECT completed");
                request.extend_from_slice(&buffer[..length]);
            }
            assert!(
                String::from_utf8_lossy(&request)
                    .starts_with("CONNECT target.invalid:80 HTTP/1.1\r\n"),
                "unexpected proxy request: {}",
                String::from_utf8_lossy(&request)
            );

            let mut upstream = TcpStream::connect(upstream_address).await.unwrap();
            downstream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            tokio::io::copy_bidirectional(&mut downstream, &mut upstream)
                .await
                .unwrap();
        });

        let proxy_url = format!("http://{proxy_address}");
        let environment = ProxyEnvironment::from_values(Some(&proxy_url), None, "");
        let connector = build_websocket_connector(&environment, Duration::from_secs(1)).unwrap();
        let request = "ws://target.invalid/v1/responses"
            .into_client_request()
            .unwrap();
        let (mut websocket, _) = tokio::time::timeout(
            Duration::from_secs(2),
            connect_websocket(connector, request, None),
        )
        .await
        .unwrap()
        .unwrap();
        websocket
            .send(Message::text("through proxy"))
            .await
            .unwrap();
        assert_eq!(
            websocket.next().await.unwrap().unwrap(),
            Message::text("through proxy")
        );
        websocket.close(None).await.unwrap();
        drop(websocket);

        tokio::time::timeout(Duration::from_secs(2), upstream)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), proxy)
            .await
            .unwrap()
            .unwrap();
    }
}
