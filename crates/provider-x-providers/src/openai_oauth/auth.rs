use std::{fmt, process::Command, sync::Arc, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::{Method, Request, StatusCode, header};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use provider_x_core::{AuthConfig, ProxyEnvironment};
use provider_x_network::{NetworkConnector, build_http_connector};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
};
use url::Url;

const ISSUER: &str = "https://auth.openai.com";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CALLBACK_PORTS: [u16; 2] = [1455, 1457];
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const RESPONSE_BODY_LIMIT: usize = 256 * 1024;
const TOKEN_REFRESH_WINDOW_SECS: u64 = 5 * 60;

type OAuthHttpClient = Client<NetworkConnector, Full<Bytes>>;

#[derive(Clone)]
pub struct OpenAiOAuthClient {
    client: OAuthHttpClient,
    issuer: Arc<str>,
}

impl fmt::Debug for OpenAiOAuthClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OpenAiOAuthClient")
    }
}

#[derive(Debug, Error)]
pub enum OpenAiOAuthError {
    #[error("OpenAI OAuth callback unavailable")]
    CallbackUnavailable,
    #[error("OpenAI OAuth login timed out")]
    LoginTimeout,
    #[error("invalid OpenAI OAuth callback")]
    InvalidCallback,
    #[error("failed to open the system browser")]
    BrowserOpen,
    #[error("failed to build the OpenAI OAuth request")]
    RequestBuild,
    #[error("OpenAI OAuth transport failed")]
    Transport,
    #[error("OpenAI OAuth response timed out")]
    ResponseTimeout,
    #[error("OpenAI OAuth returned HTTP {0}")]
    ResponseStatus(u16),
    #[error("invalid OpenAI OAuth response")]
    InvalidResponse,
    #[error("OpenAI OAuth response is missing ChatGPT account information")]
    MissingAccount,
    #[error("OpenAI OAuth credentials are invalid")]
    InvalidCredentials,
    #[error("invalid OAuth network configuration")]
    NetworkConfiguration,
}

impl OpenAiOAuthClient {
    /// Creates an `OpenAI` account OAuth client using the configured proxy environment.
    ///
    /// # Errors
    ///
    /// Returns an error when the network connector cannot be configured.
    pub fn new(connect_timeout: Duration) -> Result<Self, OpenAiOAuthError> {
        Self::with_issuer(connect_timeout, Arc::from(ISSUER))
    }

    fn with_issuer(connect_timeout: Duration, issuer: Arc<str>) -> Result<Self, OpenAiOAuthError> {
        let proxy_environment = ProxyEnvironment::read();
        let connector = build_http_connector(&proxy_environment, connect_timeout)
            .map_err(|_| OpenAiOAuthError::NetworkConfiguration)?;
        Ok(Self {
            client: Client::builder(TokioExecutor::new()).build(connector),
            issuer,
        })
    }

    /// Runs the browser-based authorization-code flow with PKCE.
    ///
    /// # Errors
    ///
    /// Returns an error when the browser flow, callback, or token exchange fails.
    pub async fn login_with_browser(&self) -> Result<AuthConfig, OpenAiOAuthError> {
        let listener = bind_callback_listener().await?;
        let port = listener
            .local_addr()
            .map_err(|_| OpenAiOAuthError::CallbackUnavailable)?
            .port();
        let redirect_uri = format!("http://localhost:{port}/auth/callback");
        let pkce = Pkce::generate()?;
        let state = random_urlsafe(32)?;
        let authorization_url = authorization_url(&redirect_uri, &pkce.challenge, &state)?;
        open_browser(authorization_url.as_str())?;

        let (code, mut browser) =
            tokio::time::timeout(LOGIN_TIMEOUT, receive_authorization_code(listener, &state))
                .await
                .map_err(|_| OpenAiOAuthError::LoginTimeout)??;
        let tokens = match self
            .exchange_authorization_code(&redirect_uri, &pkce.verifier, &code)
            .await
        {
            Ok(tokens) => tokens,
            Err(error) => {
                let _ = write_browser_response(
                    &mut browser,
                    StatusCode::BAD_GATEWAY,
                    "OpenAI sign-in could not be completed. Return to ProviderX and try again.",
                )
                .await;
                return Err(error);
            }
        };
        let auth = match auth_from_tokens(tokens) {
            Ok(auth) => auth,
            Err(error) => {
                let _ = write_browser_response(
                    &mut browser,
                    StatusCode::BAD_GATEWAY,
                    "OpenAI sign-in could not be completed. Return to ProviderX and try again.",
                )
                .await;
                return Err(error);
            }
        };
        write_browser_response(
            &mut browser,
            StatusCode::OK,
            "OpenAI sign-in completed. You can close this window and return to ProviderX.",
        )
        .await?;
        Ok(auth)
    }

    /// Refreshes an existing `OpenAI` account credential and preserves rotating tokens.
    ///
    /// # Errors
    ///
    /// Returns an error when the credential is invalid or the refresh request fails.
    pub async fn refresh(&self, auth: &AuthConfig) -> Result<AuthConfig, OpenAiOAuthError> {
        let AuthConfig::OpenAiOAuth {
            refresh_token,
            account_id,
            email,
            is_fedramp,
            ..
        } = auth
        else {
            return Err(OpenAiOAuthError::InvalidCredentials);
        };
        let request = RefreshRequest {
            client_id: CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token,
        };
        let body = serde_json::to_vec(&request).map_err(|_| OpenAiOAuthError::RequestBuild)?;
        let response: RefreshResponse = self
            .post(
                &format!("{}/oauth/token", self.issuer.trim_end_matches('/')),
                "application/json",
                body,
            )
            .await?;
        let access_token = response
            .access_token
            .ok_or(OpenAiOAuthError::InvalidResponse)?;
        let claims = response
            .id_token
            .as_deref()
            .map(parse_identity_claims)
            .transpose()?
            .flatten();
        let refreshed_account_id = claims
            .as_ref()
            .and_then(|claims| claims.account_id.as_deref())
            .unwrap_or(account_id)
            .to_owned();
        let refreshed_email = claims
            .as_ref()
            .and_then(|claims| claims.email.clone())
            .or_else(|| email.clone());
        let refreshed_is_fedramp = claims
            .as_ref()
            .map_or(*is_fedramp, |claims| claims.is_fedramp);
        Ok(AuthConfig::OpenAiOAuth {
            expires_at_unix: parse_expiration(&access_token)?,
            access_token,
            refresh_token: response
                .refresh_token
                .unwrap_or_else(|| refresh_token.clone()),
            account_id: refreshed_account_id,
            email: refreshed_email,
            is_fedramp: refreshed_is_fedramp,
        })
    }

    async fn exchange_authorization_code(
        &self,
        redirect_uri: &str,
        verifier: &str,
        code: &str,
    ) -> Result<TokenResponse, OpenAiOAuthError> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("client_id", CLIENT_ID)
            .append_pair("code_verifier", verifier)
            .finish()
            .into_bytes();
        self.post(
            &format!("{}/oauth/token", self.issuer.trim_end_matches('/')),
            "application/x-www-form-urlencoded",
            body,
        )
        .await
    }

    async fn post<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
        content_type: &'static str,
        body: Vec<u8>,
    ) -> Result<T, OpenAiOAuthError> {
        let request = Request::builder()
            .method(Method::POST)
            .uri(url)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::ACCEPT, "application/json")
            .header(
                header::USER_AGENT,
                concat!("codex_cli_rs/", env!("CARGO_PKG_VERSION")),
            )
            .header("originator", "codex_cli_rs")
            .body(Full::new(Bytes::from(body)))
            .map_err(|_| OpenAiOAuthError::RequestBuild)?;
        let response = tokio::time::timeout(REQUEST_TIMEOUT, self.client.request(request))
            .await
            .map_err(|_| OpenAiOAuthError::ResponseTimeout)?
            .map_err(|_| OpenAiOAuthError::Transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(OpenAiOAuthError::ResponseStatus(status.as_u16()));
        }
        let body = tokio::time::timeout(
            REQUEST_TIMEOUT,
            Limited::new(response.into_body(), RESPONSE_BODY_LIMIT).collect(),
        )
        .await
        .map_err(|_| OpenAiOAuthError::ResponseTimeout)?
        .map_err(|_| OpenAiOAuthError::InvalidResponse)?
        .to_bytes();
        serde_json::from_slice(&body).map_err(|_| OpenAiOAuthError::InvalidResponse)
    }
}

#[must_use]
pub fn needs_refresh(auth: &AuthConfig, now_unix: u64) -> bool {
    auth.openai_oauth_expires_at_unix()
        .is_some_and(|expires_at| expires_at <= now_unix.saturating_add(TOKEN_REFRESH_WINDOW_SECS))
}

#[derive(Debug)]
struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    fn generate() -> Result<Self, OpenAiOAuthError> {
        let verifier = random_urlsafe(64)?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Ok(Self {
            verifier,
            challenge,
        })
    }
}

#[derive(Deserialize)]
#[allow(clippy::struct_field_names)] // OAuth uses these standard response field names.
struct TokenResponse {
    id_token: String,
    access_token: String,
    refresh_token: String,
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    client_id: &'static str,
    grant_type: &'static str,
    refresh_token: &'a str,
}

#[derive(Deserialize)]
#[allow(clippy::struct_field_names)] // Refresh responses use the same standard token names.
struct RefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

#[derive(Debug, Default)]
struct IdentityClaims {
    account_id: Option<String>,
    email: Option<String>,
    is_fedramp: bool,
}

fn auth_from_tokens(tokens: TokenResponse) -> Result<AuthConfig, OpenAiOAuthError> {
    let identity =
        parse_identity_claims(&tokens.id_token)?.ok_or(OpenAiOAuthError::MissingAccount)?;
    let account_id = identity
        .account_id
        .ok_or(OpenAiOAuthError::MissingAccount)?;
    Ok(AuthConfig::OpenAiOAuth {
        expires_at_unix: parse_expiration(&tokens.access_token)?,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        account_id,
        email: identity.email,
        is_fedramp: identity.is_fedramp,
    })
}

fn authorization_url(
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> Result<Url, OpenAiOAuthError> {
    let mut url = Url::parse(&format!("{ISSUER}/oauth/authorize"))
        .map_err(|_| OpenAiOAuthError::RequestBuild)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair(
            "scope",
            "openid profile email offline_access api.connectors.read api.connectors.invoke",
        )
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", "codex_cli_rs");
    Ok(url)
}

async fn bind_callback_listener() -> Result<TcpListener, OpenAiOAuthError> {
    for port in CALLBACK_PORTS {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
            return Ok(listener);
        }
    }
    Err(OpenAiOAuthError::CallbackUnavailable)
}

async fn receive_authorization_code(
    listener: TcpListener,
    expected_state: &str,
) -> Result<(String, TcpStream), OpenAiOAuthError> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|_| OpenAiOAuthError::InvalidCallback)?;
        let target = read_request_target(&mut stream).await?;
        let parsed = Url::parse(&format!("http://localhost{target}"))
            .map_err(|_| OpenAiOAuthError::InvalidCallback)?;
        if parsed.path() != "/auth/callback" {
            write_browser_response(&mut stream, StatusCode::NOT_FOUND, "Not Found").await?;
            continue;
        }
        let mut state = None;
        let mut code = None;
        let mut error = None;
        for (name, value) in parsed.query_pairs() {
            match name.as_ref() {
                "state" => state = Some(value.into_owned()),
                "code" => code = Some(value.into_owned()),
                "error" => error = Some(value.into_owned()),
                _ => {}
            }
        }
        if error.is_some() || state.as_deref() != Some(expected_state) || code.is_none() {
            write_browser_response(
                &mut stream,
                StatusCode::BAD_REQUEST,
                "OpenAI sign-in could not be completed. Return to ProviderX and try again.",
            )
            .await?;
            return Err(OpenAiOAuthError::InvalidCallback);
        }
        return code
            .map(|code| (code, stream))
            .ok_or(OpenAiOAuthError::InvalidCallback);
    }
}

async fn read_request_target(stream: &mut TcpStream) -> Result<String, OpenAiOAuthError> {
    let mut bytes = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    while bytes.len() < 16 * 1024 {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|_| OpenAiOAuthError::InvalidCallback)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let request = std::str::from_utf8(&bytes).map_err(|_| OpenAiOAuthError::InvalidCallback)?;
    let mut request_line = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace();
    match (
        request_line.next(),
        request_line.next(),
        request_line.next(),
    ) {
        (Some("GET"), Some(target), Some(version)) if version.starts_with("HTTP/") => {
            Ok(target.to_owned())
        }
        _ => Err(OpenAiOAuthError::InvalidCallback),
    }
}

async fn write_browser_response(
    stream: &mut TcpStream,
    status: StatusCode,
    message: &str,
) -> Result<(), OpenAiOAuthError> {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>ProviderX</title><body><p>{message}</p></body>"
    );
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("Response"),
        body.len(),
        body
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|_| OpenAiOAuthError::InvalidCallback)
}

fn parse_identity_claims(jwt: &str) -> Result<Option<IdentityClaims>, OpenAiOAuthError> {
    let payload = decode_jwt_payload(jwt)?;
    let auth = payload
        .get("https://api.openai.com/auth")
        .and_then(serde_json::Value::as_object);
    let account_id = auth
        .and_then(|claims| claims.get("chatgpt_account_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let is_fedramp = auth
        .and_then(|claims| claims.get("chatgpt_account_is_fedramp"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let email = payload
        .get("email")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            payload
                .get("https://api.openai.com/profile")
                .and_then(serde_json::Value::as_object)
                .and_then(|profile| profile.get("email"))
                .and_then(serde_json::Value::as_str)
        })
        .map(str::to_owned);
    Ok(
        (account_id.is_some() || email.is_some()).then_some(IdentityClaims {
            account_id,
            email,
            is_fedramp,
        }),
    )
}

fn parse_expiration(jwt: &str) -> Result<u64, OpenAiOAuthError> {
    decode_jwt_payload(jwt)?
        .get("exp")
        .and_then(serde_json::Value::as_u64)
        .filter(|expiration| *expiration > 0)
        .ok_or(OpenAiOAuthError::InvalidResponse)
}

fn decode_jwt_payload(jwt: &str) -> Result<serde_json::Value, OpenAiOAuthError> {
    let mut parts = jwt.split('.');
    let (_header, payload, _signature) = match (parts.next(), parts.next(), parts.next()) {
        (Some(header), Some(payload), Some(signature))
            if !header.is_empty() && !payload.is_empty() && !signature.is_empty() =>
        {
            (header, payload, signature)
        }
        _ => return Err(OpenAiOAuthError::InvalidResponse),
    };
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| OpenAiOAuthError::InvalidResponse)?;
    serde_json::from_slice(&decoded).map_err(|_| OpenAiOAuthError::InvalidResponse)
}

fn random_urlsafe(byte_count: usize) -> Result<String, OpenAiOAuthError> {
    let mut bytes = vec![0_u8; byte_count];
    getrandom::fill(&mut bytes).map_err(|_| OpenAiOAuthError::RequestBuild)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn open_browser(url: &str) -> Result<(), OpenAiOAuthError> {
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|_| OpenAiOAuthError::BrowserOpen)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = url;
        Err(OpenAiOAuthError::BrowserOpen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(payload: &serde_json::Value) -> String {
        format!(
            "e30.{}.signature",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        )
    }

    #[test]
    fn authorization_uses_current_codex_pkce_contract() {
        let url =
            authorization_url("http://localhost:1455/auth/callback", "challenge", "state").unwrap();
        let query = url
            .query_pairs()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(url.path(), "/oauth/authorize");
        assert_eq!(query.get("client_id").unwrap().as_ref(), CLIENT_ID);
        assert_eq!(query.get("code_challenge_method").unwrap().as_ref(), "S256");
        assert_eq!(query.get("state").unwrap().as_ref(), "state");
        assert!(
            query
                .get("scope")
                .is_some_and(|scope| scope.contains("offline_access"))
        );
    }

    #[test]
    fn token_material_is_parsed_without_debug_exposure() {
        let id_token = jwt(&serde_json::json!({
            "email": "person@example.com",
            "https://api.openai.com/auth": {"chatgpt_account_id": "account-123"}
        }));
        let access_token = jwt(&serde_json::json!({"exp": 1_900_000_000_u64}));
        let auth = auth_from_tokens(TokenResponse {
            id_token,
            access_token,
            refresh_token: "refresh-secret".to_owned(),
        })
        .unwrap();
        assert_eq!(auth.openai_oauth_expires_at_unix(), Some(1_900_000_000));
        let diagnostic = format!("{auth:?}");
        assert!(!diagnostic.contains("refresh-secret"));
        assert!(!diagnostic.contains("account-123"));
        assert!(!diagnostic.contains("person@example.com"));
    }

    #[test]
    fn refresh_window_is_five_minutes() {
        let auth = AuthConfig::OpenAiOAuth {
            access_token: "access".to_owned(),
            refresh_token: "refresh".to_owned(),
            account_id: "account".to_owned(),
            expires_at_unix: 1_000,
            email: None,
            is_fedramp: false,
        };
        assert!(needs_refresh(&auth, 700));
        assert!(!needs_refresh(&auth, 699));
    }

    #[tokio::test]
    async fn refresh_rotates_tokens_and_updates_account_metadata() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let access_token = jwt(&serde_json::json!({"exp": 1_900_000_100_u64}));
        let id_token = jwt(&serde_json::json!({
            "email": "new@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "new-account",
                "chatgpt_account_is_fedramp": true
            }
        }));
        let response_body = serde_json::to_vec(&serde_json::json!({
            "id_token": id_token,
            "access_token": access_token,
            "refresh_token": "new-refresh"
        }))
        .unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 1024];
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&chunk[..read]);
                let Some(headers_end) = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|position| position + 4)
                else {
                    continue;
                };
                let headers = std::str::from_utf8(&request[..headers_end]).unwrap();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                    })
                    .unwrap();
                if request.len() >= headers_end + content_length {
                    break;
                }
            }
            let request = String::from_utf8(request).unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(&response_body).await.unwrap();
            request
        });
        let client = OpenAiOAuthClient::with_issuer(
            Duration::from_secs(1),
            Arc::from(format!("http://{address}")),
        )
        .unwrap();
        let old = AuthConfig::OpenAiOAuth {
            access_token: "old-access".to_owned(),
            refresh_token: "old-refresh".to_owned(),
            account_id: "old-account".to_owned(),
            expires_at_unix: 1,
            email: Some("old@example.com".to_owned()),
            is_fedramp: false,
        };

        let refreshed = client.refresh(&old).await.unwrap();
        let AuthConfig::OpenAiOAuth {
            access_token,
            refresh_token,
            account_id,
            expires_at_unix,
            email,
            is_fedramp,
        } = refreshed
        else {
            panic!("expected OAuth credentials");
        };
        assert!(access_token.starts_with("e30."));
        assert_eq!(refresh_token, "new-refresh");
        assert_eq!(account_id, "new-account");
        assert_eq!(expires_at_unix, 1_900_000_100);
        assert_eq!(email.as_deref(), Some("new@example.com"));
        assert!(is_fedramp);

        let request = server.await.unwrap();
        assert!(request.starts_with("POST /oauth/token HTTP/1.1"));
        assert!(request.contains("\"grant_type\":\"refresh_token\""));
        assert!(request.contains("\"refresh_token\":\"old-refresh\""));
        assert!(request.contains("originator: codex_cli_rs"));
    }
}
