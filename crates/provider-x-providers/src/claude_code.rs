//! Local Claude Code execution. Each request owns one bounded process and an isolated MCP
//! endpoint. Caller tools are returned to the caller; this process never executes them locally.
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    Request, Response, StatusCode,
    body::{Body, Frame, Incoming},
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    path::PathBuf,
    pin::Pin,
    process::Stdio,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    process::Command,
    sync::mpsc,
    task::{JoinHandle, JoinSet},
};

#[derive(Debug, thiserror::Error)]
pub enum ClaudeCodeError {
    #[error("Claude Code is not installed; install it and run claude auth login")]
    NotInstalled,
    #[error("Claude Code could not be started")]
    Start,
    #[error("Claude Code input is unsupported or exceeds its configured limit")]
    InvalidRequest,
    #[error("Claude Code returned an invalid or incomplete stream")]
    InvalidStream,
    #[error("Claude Code stream timed out")]
    Timeout,
}

/// Locates a locally installed CLI without reading credentials or invoking it.
#[must_use]
pub fn executable() -> Option<PathBuf> {
    let mut paths: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("claude"))
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home).join(".local/bin/claude"));
    }
    paths.extend([
        PathBuf::from("/opt/homebrew/bin/claude"),
        PathBuf::from("/usr/local/bin/claude"),
    ]);
    paths.into_iter().find(|path| path.is_file())
}

/// Dropping a consumer cancels the process, its stdin writer and its MCP listener.
pub struct ClaudeCodeBody {
    receiver: mpsc::Receiver<Result<Bytes, ClaudeCodeError>>,
    task: JoinHandle<()>,
}
impl Drop for ClaudeCodeBody {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Body for ClaudeCodeBody {
    type Data = Bytes;
    type Error = ClaudeCodeError;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        self.receiver
            .poll_recv(cx)
            .map(|frame| frame.map(|result| result.map(Frame::data)))
    }
}
struct AbortTask(JoinHandle<()>);
impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Starts an isolated local CLI turn from a prepared Anthropic request.
/// # Errors
/// Returns a redacted error for unavailable dependencies or malformed input.
pub async fn start(
    body: &[u8],
    limit: usize,
    timeout: Duration,
) -> Result<ClaudeCodeBody, ClaudeCodeError> {
    start_with(
        executable().ok_or(ClaudeCodeError::NotInstalled)?,
        body,
        limit,
        timeout,
    )
    .await
}

#[allow(clippy::too_many_lines)] // Own process, callback listener and cleanup guards in one scope.
async fn start_with(
    binary: PathBuf,
    body: &[u8],
    limit: usize,
    timeout: Duration,
) -> Result<ClaudeCodeBody, ClaudeCodeError> {
    if body.len() > limit {
        return Err(ClaudeCodeError::InvalidRequest);
    }
    let request: Value =
        serde_json::from_slice(body).map_err(|_| ClaudeCodeError::InvalidRequest)?;
    let model = request["model"]
        .as_str()
        .ok_or(ClaudeCodeError::InvalidRequest)?;
    let messages = request["messages"]
        .as_array()
        .ok_or(ClaudeCodeError::InvalidRequest)?;
    for message in messages {
        if let Some(blocks) = message["content"].as_array() {
            for block in blocks {
                match block["type"].as_str() {
                    Some("text" | "thinking" | "redacted_thinking" | "tool_use") => {}
                    Some("tool_result")
                        if block.get("content").is_none_or(|value| {
                            value.is_string()
                                || value.as_array().is_some_and(|items| {
                                    items.iter().all(|item| item["type"] == "text")
                                })
                        }) => {}
                    _ => return Err(ClaudeCodeError::InvalidRequest),
                }
            }
        }
    }
    let tools = request
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // The genuine CLI retains its own system prompt. External instructions and complete
    // caller-owned history are user context. No hidden local conversation is replayed.
    let prompt =
        json!({"external_system_instructions":request.get("system"),"conversation":messages});
    let input = format!(
        "{}\n",
        json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":prompt.to_string()}]}})
    );
    if input.len() > limit.saturating_mul(2) {
        return Err(ClaudeCodeError::InvalidRequest);
    }
    let directory = tempfile::tempdir().map_err(|_| ClaudeCodeError::Start)?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| ClaudeCodeError::Start)?;
    let address = listener.local_addr().map_err(|_| ClaudeCodeError::Start)?;
    let mut token = [0_u8; 32];
    getrandom::fill(&mut token).map_err(|_| ClaudeCodeError::Start)?;
    let authorization = format!("Bearer {}", hex::encode(token));
    let config = json!({"mcpServers":{"providerx":{"type":"http","url":format!("http://{address}/mcp"),"headers":{"Authorization":authorization}}}});
    // Keep the capability and tool definitions out of argv and diagnostics.
    let config_path = directory.path().join("mcp.json");
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&config_path)
            .map_err(|_| ClaudeCodeError::Start)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|_| ClaudeCodeError::Start)?;
        }
        file.write_all(config.to_string().as_bytes())
            .map_err(|_| ClaudeCodeError::Start)?;
    }
    let mut command = Command::new(binary);
    command
        .args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
            "--verbose",
            "--model",
            model,
            "--tools",
            "",
            "--strict-mcp-config",
            "--mcp-config",
        ])
        .arg(&config_path)
        .args([
            "--setting-sources",
            "",
            "--permission-mode",
            "dontAsk",
            "--allowedTools",
            "mcp__providerx__*",
            "--max-turns",
            "1",
            "--no-session-persistence",
        ])
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for key in [
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDECODE",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_SSE_PORT",
    ] {
        command.env_remove(key);
    }
    command
        .env("ENABLE_CLAUDEAI_MCP_SERVERS", "0")
        .env("DISABLE_AUTO_COMPACT", "1")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
    if let Some(effort) = request
        .pointer("/output_config/effort")
        .and_then(Value::as_str)
    {
        command.args(["--effort", effort]);
    }
    let mut child = command.spawn().map_err(|_| ClaudeCodeError::Start)?;
    let mut stdin = child.stdin.take().ok_or(ClaudeCodeError::Start)?;
    let stdout = child.stdout.take().ok_or(ClaudeCodeError::Start)?;
    let writer = AbortTask(tokio::spawn(async move {
        let _ = stdin.write_all(input.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }));
    let mcp = AbortTask(tokio::spawn(serve_tools(
        listener,
        authorization,
        tools,
        limit,
    )));
    let (sender, receiver) = mpsc::channel(8);
    let task = tokio::spawn(async move {
        let _directory = directory;
        let _writer = writer;
        let _mcp = mcp;
        let result = pump(stdout, limit, timeout, &sender).await;
        if let Err(error) = result {
            let _ = sender.send(Err(error)).await;
        }
        let _ = child.kill().await;
        let _ = child.wait().await;
    });
    Ok(ClaudeCodeBody { receiver, task })
}

async fn pump(
    stdout: tokio::process::ChildStdout,
    limit: usize,
    timeout: Duration,
    sender: &mpsc::Sender<Result<Bytes, ClaudeCodeError>>,
) -> Result<(), ClaudeCodeError> {
    let mut reader = BufReader::new(stdout);
    let mut started = false;
    loop {
        let line = tokio::select! {
            () = sender.closed() => return Ok(()),
            result = tokio::time::timeout(timeout, bounded_line(&mut reader, limit)) => result.map_err(|_| ClaudeCodeError::Timeout)??,
        };
        if line.is_empty() {
            return Err(ClaudeCodeError::InvalidStream);
        }
        let envelope: Value =
            serde_json::from_slice(&line).map_err(|_| ClaudeCodeError::InvalidStream)?;
        if envelope["type"] != "stream_event" {
            if envelope["type"] == "result" {
                return Err(ClaudeCodeError::InvalidStream);
            }
            continue;
        }
        let mut event = envelope["event"].clone();
        let kind = event["type"]
            .as_str()
            .ok_or(ClaudeCodeError::InvalidStream)?
            .to_owned();
        if kind == "message_start" {
            if started {
                return Err(ClaudeCodeError::InvalidStream);
            }
            started = true;
        }
        if !started {
            return Err(ClaudeCodeError::InvalidStream);
        }
        if kind == "content_block_start" && event["content_block"]["type"] == "tool_use" {
            let name = event["content_block"]["name"]
                .as_str()
                .ok_or(ClaudeCodeError::InvalidStream)?;
            let name = name
                .strip_prefix("mcp__providerx__")
                .ok_or(ClaudeCodeError::InvalidStream)?
                .to_owned();
            event["content_block"]["name"] = Value::String(name);
        }
        if sender
            .send(Ok(Bytes::from(format!("data: {event}\n\n"))))
            .await
            .is_err()
        {
            return Ok(());
        }
        if kind == "message_stop" {
            return Ok(());
        }
    }
}

async fn bounded_line(
    reader: &mut BufReader<tokio::process::ChildStdout>,
    limit: usize,
) -> Result<Vec<u8>, ClaudeCodeError> {
    let mut line = Vec::new();
    loop {
        let buffer = reader
            .fill_buf()
            .await
            .map_err(|_| ClaudeCodeError::InvalidStream)?;
        if buffer.is_empty() {
            return Ok(line);
        }
        let end = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1);
        let take = end.unwrap_or(buffer.len());
        if line.len().saturating_add(take) > limit {
            return Err(ClaudeCodeError::InvalidStream);
        }
        line.extend_from_slice(&buffer[..take]);
        reader.consume(take);
        if end.is_some() {
            return Ok(line);
        }
    }
}

async fn serve_tools(
    listener: TcpListener,
    authorization: String,
    tools: Vec<Value>,
    limit: usize,
) {
    let tools: Vec<Value> = tools.into_iter().map(|tool| json!({"name":tool["name"],"description":tool.get("description").and_then(Value::as_str).unwrap_or(""),"inputSchema":tool["input_schema"]})).collect();
    let tools = Arc::new(tools);
    let authorization = Arc::new(authorization);
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            incoming = listener.accept() => {
                let Ok((stream, _)) = incoming else { return };
                if connections.len() >= 8 { continue; }
                let authorization = Arc::clone(&authorization); let tools = Arc::clone(&tools);
                connections.spawn(async move {
                    let service = service_fn(move |request| mcp_request(request, Arc::clone(&authorization), Arc::clone(&tools), limit));
                    let _ = tokio::time::timeout(Duration::from_secs(10), hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service)).await;
                });
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

async fn mcp_request(
    request: Request<Incoming>,
    authorization: Arc<String>,
    tools: Arc<Vec<Value>>,
    limit: usize,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let reply = |status, value: Value| {
        Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(value.to_string())))
            .expect("static response")
    };
    if request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        != Some(authorization.as_str())
        || request.headers().contains_key("origin")
    {
        return Ok(reply(StatusCode::UNAUTHORIZED, Value::Null));
    }
    if request.method() != hyper::Method::POST || request.uri().path() != "/mcp" {
        return Ok(reply(StatusCode::METHOD_NOT_ALLOWED, Value::Null));
    }
    let Ok(body) = Limited::new(request.into_body(), limit).collect().await else {
        return Ok(reply(StatusCode::BAD_REQUEST, Value::Null));
    };
    let Ok(rpc) = serde_json::from_slice::<Value>(&body.to_bytes()) else {
        return Ok(reply(StatusCode::BAD_REQUEST, Value::Null));
    };
    if rpc.get("id").is_none() {
        return Ok(reply(StatusCode::ACCEPTED, Value::Null));
    }
    let result = match rpc["method"].as_str() {
        Some("initialize") => {
            json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"ProviderX caller tools","version":"1"}})
        }
        Some("tools/list") => json!({"tools":*tools}),
        // Tool execution belongs to the downstream agent. Never execute CLI-local tools.
        Some("tools/call") => {
            json!({"isError":true,"content":[{"type":"text","text":"Tool execution is delegated to the caller. End this turn."}]})
        }
        Some("ping") => json!({}),
        _ => {
            return Ok(reply(
                StatusCode::OK,
                json!({"jsonrpc":"2.0","id":rpc["id"],"error":{"code":-32601,"message":"Method not found"}}),
            ));
        }
    };
    Ok(reply(
        StatusCode::OK,
        json!({"jsonrpc":"2.0","id":rpc["id"],"result":result}),
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake(script: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        (dir, path)
    }
    const REQUEST: &[u8] = br#"{"model":"synthetic-model","messages":[{"role":"user","content":"synthetic-private-text"}],"tools":[]}"#;
    #[tokio::test]
    async fn fake_cli_streams_native_events_and_removes_mcp_prefix() {
        let (_dir, binary) = fake(
            r#"
read -r request
printf '%s\n' '{"type":"system","subtype":"init"}'
printf '%s\n' '{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg-test","usage":{"input_tokens":2}}}}'
printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call-test","name":"mcp__providerx__lookup","input":{}}}}'
printf '%s\n' '{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}}'
printf '%s\n' '{"type":"stream_event","event":{"type":"message_stop"}}'
exec /bin/sleep 30
"#,
        );
        let body = start_with(binary, REQUEST, 4096, Duration::from_secs(2))
            .await
            .unwrap();
        let bytes = body.collect().await.unwrap().to_bytes();
        let output = std::str::from_utf8(&bytes).unwrap();
        assert!(output.contains("\"name\":\"lookup\""));
        assert!(!output.contains("mcp__"));
        assert!(!output.contains("synthetic-private-text"));
        assert!(output.contains("message_stop"));
    }
    #[tokio::test]
    async fn malformed_and_oversized_output_produce_redacted_errors() {
        let (_dir, binary) = fake("read -r request\nprintf '%s\\n' 'synthetic-private-error'\n");
        let body = start_with(binary, REQUEST, 4096, Duration::from_secs(2))
            .await
            .unwrap();
        let error = body.collect().await.unwrap_err();
        assert!(!format!("{error:?}").contains("synthetic-private"));
        let (_dir, binary) = fake("read -r request\nhead -c 8192 /dev/zero\n");
        let body = start_with(binary, REQUEST, 4096, Duration::from_secs(2))
            .await
            .unwrap();
        assert!(matches!(
            body.collect().await.unwrap_err(),
            ClaudeCodeError::InvalidStream
        ));
    }
    #[tokio::test]
    async fn dropping_body_terminates_the_child() {
        let directory = tempfile::tempdir().unwrap();
        let pidfile = directory.path().join("pid");
        let (_dir, binary) = fake(&format!(
            "echo $$ > '{}'\nexec /bin/sleep 30",
            pidfile.display()
        ));
        let body = start_with(binary, REQUEST, 4096, Duration::from_secs(2))
            .await
            .unwrap();
        for _ in 0..100 {
            if pidfile.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pid = std::fs::read_to_string(pidfile).unwrap();
        drop(body);
        for _ in 0..100 {
            let status = Command::new("/bin/kill")
                .args(["-0", pid.trim()])
                .stderr(Stdio::null())
                .status()
                .await
                .unwrap();
            if !status.success() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("fake child was not terminated");
    }
    #[tokio::test]
    async fn timeout_and_truncated_stream_fail_without_http_fallback() {
        let (_dir, binary) = fake("exec /bin/sleep 30");
        let body = start_with(binary, REQUEST, 4096, Duration::from_millis(50))
            .await
            .unwrap();
        assert!(matches!(
            body.collect().await.unwrap_err(),
            ClaudeCodeError::Timeout
        ));
        let (_dir, binary) = fake("exit 1");
        let body = start_with(binary, REQUEST, 4096, Duration::from_secs(1))
            .await
            .unwrap();
        assert!(matches!(
            body.collect().await.unwrap_err(),
            ClaudeCodeError::InvalidStream
        ));
    }

    #[tokio::test]
    async fn images_are_rejected_instead_of_serialized_as_text() {
        let result = start_with(PathBuf::from("/nonexistent-synthetic-cli"),
            br#"{"model":"test","messages":[{"role":"user","content":[{"type":"image","source":{"data":"synthetic"}}]}]}"#,
            4096, Duration::from_secs(1)).await;
        assert!(matches!(result, Err(ClaudeCodeError::InvalidRequest)));
    }
}
