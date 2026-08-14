use std::collections::{BTreeMap, VecDeque};

use bytes::Bytes;
use provider_x_protocol::{
    BridgeFailure, WsHttpAction, WsHttpEventDecoder, WsHttpProtocolAdapter, WsHttpStreamOutcome,
};
use serde_json::{Map, Value, json};

use crate::ChatProtocolError;

const MAX_HISTORY_ITEMS: usize = 256;
const MAX_STREAM_TOOL_CALLS: usize = 256;

#[derive(Default)]
struct WarmupDefaults {
    instructions: Option<Value>,
    tools: Option<Value>,
    parallel_tool_calls: Option<Value>,
    tool_choice: Option<Value>,
    reasoning: Option<Value>,
}

pub struct ResponsesChatBridgeSession {
    upstream_model: String,
    warmup_response_id: Option<String>,
    warmup: WarmupDefaults,
    history: Vec<Value>,
    tool_names: BTreeMap<String, ToolIdentity>,
    max_history_bytes: usize,
}

pub enum ChatBridgeAction {
    Warmup { events: Vec<String> },
    Request(ChatBridgeRequest),
}

#[derive(Debug)]
pub struct ChatBridgeRequest {
    pub body: Bytes,
    pub messages: Vec<Value>,
    pub tool_names: BTreeMap<String, ToolIdentity>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolIdentity {
    pub name: String,
    pub namespace: Option<String>,
}

pub struct ChatCompletionsWsHttpAdapter {
    session: ResponsesChatBridgeSession,
}

impl ChatBridgeRequest {
    #[must_use]
    pub fn decoder(&self, max_buffer_bytes: usize) -> ChatSseDecoder {
        ChatSseDecoder::with_tool_names(max_buffer_bytes, self.tool_names.clone())
    }
}

impl WsHttpProtocolAdapter for ChatCompletionsWsHttpAdapter {
    type Pending = Vec<Value>;
    type Commit = Value;
    type Decoder = ChatSseDecoder;

    fn new_session(upstream_model: String, max_session_bytes: usize) -> Self {
        Self {
            session: ResponsesChatBridgeSession::new(upstream_model, max_session_bytes),
        }
    }

    fn upstream_url(http_endpoint: &str) -> String {
        crate::chat_completions_url(http_endpoint)
    }

    fn prepare_action(
        &mut self,
        response_create: &str,
    ) -> Result<WsHttpAction<Self::Pending>, BridgeFailure> {
        match self
            .session
            .prepare(response_create)
            .map_err(|error| map_failure(&error))?
        {
            ChatBridgeAction::Warmup { events } => Ok(WsHttpAction::Warmup { events }),
            ChatBridgeAction::Request(request) => Ok(WsHttpAction::Request {
                body: request.body,
                pending: request.messages,
            }),
        }
    }

    fn new_decoder(&self, max_buffer_bytes: usize) -> Self::Decoder {
        ChatSseDecoder::with_tool_names(max_buffer_bytes, self.session.tool_names.clone())
    }

    fn commit_outcome(
        &mut self,
        pending: Self::Pending,
        commit: Self::Commit,
    ) -> Result<(), BridgeFailure> {
        self.session
            .commit(pending, commit)
            .map_err(|error| map_failure(&error))
    }
}

impl ResponsesChatBridgeSession {
    #[must_use]
    pub fn new(upstream_model: String, max_history_bytes: usize) -> Self {
        Self {
            upstream_model,
            warmup_response_id: None,
            warmup: WarmupDefaults::default(),
            history: Vec::new(),
            tool_names: BTreeMap::new(),
            max_history_bytes,
        }
    }

    /// Converts one Responses WebSocket `response.create` into Chat Completions JSON.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or unsupported Responses input and bounded history overflow.
    pub fn prepare(&mut self, text: &str) -> Result<ChatBridgeAction, ChatProtocolError> {
        let mut create = parse_object(text.as_bytes())?;
        if create.get("type").and_then(Value::as_str) != Some("response.create") {
            return Err(ChatProtocolError::UnsupportedWebSocketMessage);
        }
        if create.get("generate").and_then(Value::as_bool) == Some(false) {
            self.capture_defaults(&create);
            let response_id = format!("resp_provider_x_warmup_{}", random_id());
            self.warmup_response_id = Some(response_id.clone());
            return Ok(ChatBridgeAction::Warmup {
                events: warmup_events(&response_id),
            });
        }
        self.capture_defaults(&create);
        self.apply_defaults(&mut create);
        let previous_response_id = create
            .get("previous_response_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let input = create.remove("input").unwrap_or(Value::Null);
        let current = input_to_messages(input)?;
        let replay = !self.history.is_empty()
            && (previous_response_id.is_some() || contains_tool_message(&current));
        let messages = if replay {
            bounded_merge(&self.history, &current, self.max_history_bytes)?
        } else {
            current
        };
        self.warmup_response_id = None;
        let request = build_request(
            &create,
            &self.upstream_model,
            messages,
            self.max_history_bytes,
        )?;
        self.tool_names.clone_from(&request.tool_names);
        Ok(ChatBridgeAction::Request(request))
    }

    /// Saves the exact Chat Completions context and assistant result for a tool continuation.
    ///
    /// # Errors
    ///
    /// Returns an error when the bounded session history is exceeded.
    pub fn commit(
        &mut self,
        mut messages: Vec<Value>,
        assistant_message: Value,
    ) -> Result<(), ChatProtocolError> {
        messages.push(assistant_message);
        validate_history(&messages, self.max_history_bytes)?;
        self.history = messages;
        Ok(())
    }

    fn capture_defaults(&mut self, create: &Map<String, Value>) {
        for (name, target) in [
            ("instructions", &mut self.warmup.instructions),
            ("tools", &mut self.warmup.tools),
            ("parallel_tool_calls", &mut self.warmup.parallel_tool_calls),
            ("tool_choice", &mut self.warmup.tool_choice),
            ("reasoning", &mut self.warmup.reasoning),
        ] {
            if let Some(value) = create.get(name) {
                *target = Some(value.clone());
            }
        }
    }

    fn apply_defaults(&self, create: &mut Map<String, Value>) {
        for (name, value) in [
            ("instructions", self.warmup.instructions.as_ref()),
            ("tools", self.warmup.tools.as_ref()),
            (
                "parallel_tool_calls",
                self.warmup.parallel_tool_calls.as_ref(),
            ),
            ("tool_choice", self.warmup.tool_choice.as_ref()),
            ("reasoning", self.warmup.reasoning.as_ref()),
        ] {
            if !create.contains_key(name)
                && let Some(value) = value
            {
                create.insert(name.to_owned(), value.clone());
            }
        }
    }
}

/// Converts an HTTP Responses request body to one Chat Completions request.
///
/// # Errors
///
/// Returns an error for malformed or unsupported Responses input.
pub fn prepare_http_request(
    body: &[u8],
    upstream_model: &str,
    max_bytes: usize,
) -> Result<ChatBridgeRequest, ChatProtocolError> {
    let mut create = parse_object(body)?;
    if create.get("previous_response_id").is_some() {
        return Err(ChatProtocolError::UnsupportedInput(
            "previous_response_id requires the stateful WebSocket bridge".to_owned(),
        ));
    }
    let input = create.remove("input").unwrap_or(Value::Null);
    let messages = input_to_messages(input)?;
    build_request(&create, upstream_model, messages, max_bytes)
}

fn build_request(
    create: &Map<String, Value>,
    upstream_model: &str,
    mut messages: Vec<Value>,
    max_bytes: usize,
) -> Result<ChatBridgeRequest, ChatProtocolError> {
    if let Some(instructions) = create.get("instructions") {
        let text = value_as_text(instructions, "instructions")?;
        let already_present = messages.first().is_some_and(|message| {
            message.get("role").and_then(Value::as_str) == Some("system")
                && message.get("content").and_then(Value::as_str) == Some(text.as_str())
        });
        if !text.is_empty() && !already_present {
            messages.insert(0, json!({"role": "system", "content": text}));
        }
    }
    if messages.is_empty() {
        return Err(ChatProtocolError::UnsupportedInput(
            "Chat Completions requires at least one message".to_owned(),
        ));
    }
    validate_history(&messages, max_bytes)?;
    let mut request = Map::new();
    request.insert("model".to_owned(), Value::String(upstream_model.to_owned()));
    request.insert("messages".to_owned(), Value::Array(messages.clone()));
    request.insert("stream".to_owned(), Value::Bool(true));
    request.insert("stream_options".to_owned(), json!({"include_usage": true}));
    let mut tool_names = BTreeMap::new();
    if let Some(tools) = create.get("tools") {
        let converted = convert_tools(tools)?;
        tool_names = converted.names;
        if !converted.tools.is_empty() {
            request.insert("tools".to_owned(), Value::Array(converted.tools));
        }
    }
    if let Some(choice) = create.get("tool_choice")
        && !tool_names.is_empty()
        && let Some(choice) = convert_tool_choice(choice, &tool_names)?
    {
        request.insert("tool_choice".to_owned(), choice);
    }
    if !tool_names.is_empty()
        && let Some(value) = create.get("parallel_tool_calls")
    {
        request.insert("parallel_tool_calls".to_owned(), value.clone());
    }
    if let Some(value) = create.get("max_output_tokens") {
        request.insert("max_tokens".to_owned(), value.clone());
    }
    if let Some(reasoning) = create.get("reasoning").and_then(Value::as_object) {
        let effort = reasoning
            .get("effort")
            .and_then(Value::as_str)
            .map(|effort| match effort {
                "medium" => "high",
                "xhigh" => "max",
                other => other,
            });
        if let Some(effort) = effort {
            request.insert(
                "reasoning_effort".to_owned(),
                Value::String(effort.to_owned()),
            );
            request.insert("thinking".to_owned(), json!({"type": "enabled"}));
        }
    }
    let body = serde_json::to_vec(&Value::Object(request))
        .map_err(|error| ChatProtocolError::Serialization(error.to_string()))?;
    if body.len() > max_bytes {
        return Err(ChatProtocolError::SessionHistoryLimit);
    }
    Ok(ChatBridgeRequest {
        body: Bytes::from(body),
        messages,
        tool_names,
    })
}

fn parse_object(input: &[u8]) -> Result<Map<String, Value>, ChatProtocolError> {
    serde_json::from_slice::<Value>(input)
        .map_err(|error| ChatProtocolError::InvalidJson(error.to_string()))?
        .as_object()
        .cloned()
        .ok_or(ChatProtocolError::BodyMustBeObject)
}

fn input_to_messages(input: Value) -> Result<Vec<Value>, ChatProtocolError> {
    let items = match input {
        Value::Null => Vec::new(),
        Value::Array(items) => items,
        Value::String(text) => return Ok(vec![json!({"role": "user", "content": text})]),
        item => vec![item],
    };
    let mut messages = Vec::new();
    let mut pending_calls = Vec::new();
    let flush_calls = |messages: &mut Vec<Value>, calls: &mut Vec<Value>| {
        if !calls.is_empty() {
            messages.push(json!({
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": std::mem::take(calls)
            }));
        }
    };
    for item in items {
        let item_type = item.get("type").and_then(Value::as_str);
        match item_type {
            Some("message") => {
                flush_calls(&mut messages, &mut pending_calls);
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                if !matches!(role, "user" | "assistant" | "system" | "developer") {
                    return Err(ChatProtocolError::UnsupportedInput(format!(
                        "message role {role}"
                    )));
                }
                let role = if role == "developer" { "system" } else { role };
                let content = item
                    .get("content")
                    .map(|content| response_content_text(content, role))
                    .transpose()?
                    .unwrap_or_default();
                messages.push(json!({"role": role, "content": content}));
            }
            Some("function_call") => {
                let call_id = item.get("call_id").and_then(Value::as_str).ok_or_else(|| {
                    ChatProtocolError::UnsupportedInput("function_call.call_id".to_owned())
                })?;
                let name = item.get("name").and_then(Value::as_str).ok_or_else(|| {
                    ChatProtocolError::UnsupportedInput("function_call.name".to_owned())
                })?;
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                pending_calls.push(json!({
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments}
                }));
            }
            Some("function_call_output") => {
                flush_calls(&mut messages, &mut pending_calls);
                let call_id = item.get("call_id").and_then(Value::as_str).ok_or_else(|| {
                    ChatProtocolError::UnsupportedInput("function_call_output.call_id".to_owned())
                })?;
                let content = item
                    .get("output")
                    .map(|value| value_as_text(value, "function_call_output.output"))
                    .transpose()?
                    .unwrap_or_default();
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": content
                }));
            }
            Some(
                "reasoning" | "web_search" | "web_search_call" | "tool_search_call"
                | "tool_search_output",
            ) => {}
            None if item.is_string() => {
                flush_calls(&mut messages, &mut pending_calls);
                messages.push(json!({"role": "user", "content": item}));
            }
            other => {
                return Err(ChatProtocolError::UnsupportedInput(
                    other.unwrap_or("unknown item").to_owned(),
                ));
            }
        }
    }
    flush_calls(&mut messages, &mut pending_calls);
    Ok(messages)
}

fn response_content_text(content: &Value, role: &str) -> Result<String, ChatProtocolError> {
    if let Some(text) = content.as_str() {
        return Ok(text.to_owned());
    }
    let parts = content
        .as_array()
        .ok_or_else(|| ChatProtocolError::UnsupportedInput(format!("{role} message content")))?;
    let mut text = String::new();
    for part in parts {
        let part_type = part.get("type").and_then(Value::as_str);
        if !matches!(part_type, Some("input_text" | "output_text" | "text")) {
            return Err(ChatProtocolError::UnsupportedInput(
                part_type.unwrap_or("message content part").to_owned(),
            ));
        }
        if let Some(part_text) = part.get("text").and_then(Value::as_str) {
            text.push_str(part_text);
        }
    }
    Ok(text)
}

fn value_as_text(value: &Value, field: &str) -> Result<String, ChatProtocolError> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            if value.is_null() {
                Some(String::new())
            } else if value.is_object()
                || value.is_array()
                || value.is_boolean()
                || value.is_number()
            {
                serde_json::to_string(value).ok()
            } else {
                None
            }
        })
        .ok_or_else(|| ChatProtocolError::UnsupportedInput(field.to_owned()))
}

struct ConvertedTools {
    tools: Vec<Value>,
    names: BTreeMap<String, ToolIdentity>,
}

fn convert_tools(tools: &Value) -> Result<ConvertedTools, ChatProtocolError> {
    let tools = tools
        .as_array()
        .ok_or_else(|| ChatProtocolError::UnsupportedInput("tools".to_owned()))?;
    let mut converted = Vec::new();
    let mut names = BTreeMap::new();
    for tool in tools {
        match tool.get("type").and_then(Value::as_str) {
            Some("function") => push_function_tool(tool, None, &mut converted, &mut names)?,
            Some("namespace") => {
                let namespace = tool.get("name").and_then(Value::as_str).ok_or_else(|| {
                    ChatProtocolError::UnsupportedInput("namespace.name".to_owned())
                })?;
                let children = tool.get("tools").and_then(Value::as_array).ok_or_else(|| {
                    ChatProtocolError::UnsupportedInput("namespace.tools".to_owned())
                })?;
                for child in children {
                    if child.get("type").and_then(Value::as_str) != Some("function") {
                        return Err(ChatProtocolError::UnsupportedInput(
                            child
                                .get("type")
                                .and_then(Value::as_str)
                                .unwrap_or("namespace tool")
                                .to_owned(),
                        ));
                    }
                    push_function_tool(child, Some(namespace), &mut converted, &mut names)?;
                }
            }
            Some("web_search" | "web_search_preview" | "tool_search") => {}
            other => {
                return Err(ChatProtocolError::UnsupportedInput(
                    other.unwrap_or("tool").to_owned(),
                ));
            }
        }
    }
    Ok(ConvertedTools {
        tools: converted,
        names,
    })
}

fn push_function_tool(
    tool: &Value,
    namespace: Option<&str>,
    converted: &mut Vec<Value>,
    names: &mut BTreeMap<String, ToolIdentity>,
) -> Result<(), ChatProtocolError> {
    let original_name = tool
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ChatProtocolError::UnsupportedInput("tool.name".to_owned()))?;
    let chat_name = namespace.map_or_else(
        || original_name.to_owned(),
        |namespace| flatten_tool_name(namespace, original_name),
    );
    if names.contains_key(&chat_name) {
        return Err(ChatProtocolError::UnsupportedInput(format!(
            "duplicate tool name after Chat Completions conversion: {chat_name}"
        )));
    }
    let mut function = Map::new();
    function.insert("name".to_owned(), Value::String(chat_name.clone()));
    for name in ["description", "parameters", "strict"] {
        if let Some(value) = tool.get(name) {
            function.insert(name.to_owned(), value.clone());
        }
    }
    converted.push(json!({"type": "function", "function": function}));
    names.insert(
        chat_name,
        ToolIdentity {
            name: original_name.to_owned(),
            namespace: namespace.map(str::to_owned),
        },
    );
    Ok(())
}

fn flatten_tool_name(namespace: &str, name: &str) -> String {
    let mut flattened = String::with_capacity(namespace.len() + name.len() + 2);
    for byte in namespace.bytes().chain(*b"__").chain(name.bytes()) {
        if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
            flattened.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(flattened, "_x{byte:02x}_");
        }
    }
    flattened
}

fn convert_tool_choice(
    choice: &Value,
    tool_names: &BTreeMap<String, ToolIdentity>,
) -> Result<Option<Value>, ChatProtocolError> {
    if choice.is_string() {
        return Ok(Some(choice.clone()));
    }
    let object = choice
        .as_object()
        .ok_or_else(|| ChatProtocolError::UnsupportedInput("tool_choice".to_owned()))?;
    if matches!(
        object.get("type").and_then(Value::as_str),
        Some("web_search" | "web_search_preview" | "tool_search")
    ) {
        return Ok(None);
    }
    let name = object
        .get("name")
        .or_else(|| {
            object
                .get("function")
                .and_then(|function| function.get("name"))
        })
        .and_then(Value::as_str)
        .ok_or_else(|| ChatProtocolError::UnsupportedInput("tool_choice.name".to_owned()))?;
    let namespace = object.get("namespace").and_then(Value::as_str);
    let chat_name = match namespace {
        Some(namespace) => tool_names
            .iter()
            .find(|(_, identity)| {
                identity.name == name && identity.namespace.as_deref() == Some(namespace)
            })
            .map(|(chat_name, _)| chat_name.clone())
            .ok_or_else(|| {
                ChatProtocolError::UnsupportedInput("tool_choice namespace/name".to_owned())
            })?,
        None => name.to_owned(),
    };
    Ok(Some(
        json!({"type": "function", "function": {"name": chat_name}}),
    ))
}

fn contains_tool_message(messages: &[Value]) -> bool {
    messages
        .iter()
        .any(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
}

fn bounded_merge(
    history: &[Value],
    current: &[Value],
    max_bytes: usize,
) -> Result<Vec<Value>, ChatProtocolError> {
    let overlap = (0..=history.len().min(current.len()))
        .rev()
        .find(|&count| history[history.len() - count..] == current[..count])
        .unwrap_or(0);
    let mut merged = history.to_vec();
    merged.extend_from_slice(&current[overlap..]);
    validate_history(&merged, max_bytes)?;
    Ok(merged)
}

fn validate_history(messages: &[Value], max_bytes: usize) -> Result<(), ChatProtocolError> {
    let bytes = serde_json::to_vec(messages)
        .map_err(|error| ChatProtocolError::Serialization(error.to_string()))?;
    if messages.len() > MAX_HISTORY_ITEMS || bytes.len() > max_bytes {
        return Err(ChatProtocolError::SessionHistoryLimit);
    }
    Ok(())
}

#[derive(Default)]
struct ToolCallState {
    id: String,
    name: String,
    arguments: String,
    added: bool,
}

#[allow(clippy::struct_excessive_bools)] // Incremental SSE output has independent item lifecycles.
pub struct ChatSseDecoder {
    buffer: Vec<u8>,
    pending: VecDeque<String>,
    max_buffer_bytes: usize,
    max_aggregate_bytes: usize,
    aggregate_bytes: usize,
    response_id: Option<String>,
    reasoning_id: String,
    message_id: String,
    reasoning: String,
    text: String,
    tools: BTreeMap<u64, ToolCallState>,
    tool_names: BTreeMap<String, ToolIdentity>,
    reasoning_started: bool,
    text_started: bool,
    finish_reason: Option<String>,
    usage: Option<Value>,
    usage_bytes: usize,
    terminal: bool,
    completed: bool,
}

pub struct ChatStreamOutcome {
    pub terminal: bool,
    pub completed: bool,
    pub assistant_message: Value,
}

impl ChatSseDecoder {
    #[must_use]
    pub fn new(max_buffer_bytes: usize) -> Self {
        Self::with_tool_names(max_buffer_bytes, BTreeMap::new())
    }

    #[must_use]
    pub fn with_tool_names(
        max_buffer_bytes: usize,
        tool_names: BTreeMap<String, ToolIdentity>,
    ) -> Self {
        let seed = random_id();
        Self {
            buffer: Vec::new(),
            pending: VecDeque::new(),
            max_buffer_bytes,
            max_aggregate_bytes: max_buffer_bytes,
            aggregate_bytes: 0,
            response_id: None,
            reasoning_id: format!("rs_{seed}"),
            message_id: format!("msg_{seed}"),
            reasoning: String::new(),
            text: String::new(),
            tools: BTreeMap::new(),
            tool_names,
            reasoning_started: false,
            text_started: false,
            finish_reason: None,
            usage: None,
            usage_bytes: 0,
            terminal: false,
            completed: false,
        }
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Converts arbitrary Chat Completions SSE chunks to Responses WebSocket events.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed JSON, event-buffer overflow, or aggregate-state overflow.
    pub fn push(&mut self, data: &[u8]) -> Result<Vec<String>, ChatProtocolError> {
        if self.buffer.len().saturating_add(data.len()) > self.max_buffer_bytes {
            return Err(ChatProtocolError::StreamBufferLimit);
        }
        self.buffer.extend_from_slice(data);
        normalize_newlines(&mut self.buffer);
        while let Some(index) = self.buffer.windows(2).position(|window| window == b"\n\n") {
            let block: Vec<_> = self.buffer.drain(..index + 2).collect();
            self.parse_block(&block)?;
        }
        Ok(self.pending.drain(..).collect())
    }

    /// Flushes a final unterminated block and synthesizes a terminal event if needed.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed Chat Completions JSON.
    pub fn finish(&mut self) -> Result<Vec<String>, ChatProtocolError> {
        if !self.buffer.is_empty() {
            let block = std::mem::take(&mut self.buffer);
            self.parse_block(&block)?;
        }
        if !self.terminal && self.finish_reason.is_some() {
            self.finalize();
        }
        Ok(self.pending.drain(..).collect())
    }

    #[must_use]
    pub fn outcome(self) -> ChatStreamOutcome {
        let tool_calls = self
            .tools
            .into_values()
            .map(|tool| {
                json!({
                    "id": tool.id,
                    "type": "function",
                    "function": {"name": tool.name, "arguments": tool.arguments}
                })
            })
            .collect::<Vec<_>>();
        let mut assistant = json!({"role": "assistant", "content": self.text});
        if !self.reasoning.is_empty() {
            assistant["reasoning_content"] = Value::String(self.reasoning);
        }
        if !tool_calls.is_empty() {
            assistant["tool_calls"] = Value::Array(tool_calls);
            if self.text.is_empty() {
                assistant["content"] = Value::Null;
            }
        }
        ChatStreamOutcome {
            terminal: self.terminal,
            completed: self.completed,
            assistant_message: assistant,
        }
    }

    fn parse_block(&mut self, block: &[u8]) -> Result<(), ChatProtocolError> {
        let block = String::from_utf8_lossy(block).replace("\r\n", "\n");
        let data = block
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            return Ok(());
        }
        if data == "[DONE]" {
            self.finalize();
            return Ok(());
        }
        let value: Value =
            serde_json::from_str(&data).map_err(|_| ChatProtocolError::InvalidStream)?;
        if self.response_id.is_none() {
            let id = value
                .get("id")
                .and_then(Value::as_str)
                .map_or_else(|| format!("chatcmpl_{}", random_id()), str::to_owned);
            self.reserve_aggregate_bytes(id.len())?;
            self.response_id = Some(id.clone());
            self.emit(json!({"type": "response.created", "response": {"id": id}}));
        }
        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            let usage_bytes = usage.to_string().len();
            self.resize_aggregate_bytes(self.usage_bytes, usage_bytes)?;
            self.usage = Some(usage.clone());
            self.usage_bytes = usage_bytes;
        }
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| {
                choices
                    .iter()
                    .find(|choice| choice.get("index").and_then(Value::as_u64) == Some(0))
            })
        else {
            return Ok(());
        };
        let delta = choice.get("delta").and_then(Value::as_object);
        if let Some(reasoning) = delta
            .and_then(|delta| delta.get("reasoning_content"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            self.start_reasoning();
            self.reserve_aggregate_bytes(reasoning.len())?;
            self.reasoning.push_str(reasoning);
            self.emit(json!({
                "type": "response.reasoning_summary_text.delta",
                "item_id": self.reasoning_id,
                "output_index": 0,
                "summary_index": 0,
                "delta": reasoning
            }));
        }
        if let Some(content) = delta
            .and_then(|delta| delta.get("content"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            self.start_text();
            self.reserve_aggregate_bytes(content.len())?;
            self.text.push_str(content);
            self.emit(json!({
                "type": "response.output_text.delta",
                "item_id": self.message_id,
                "output_index": self.message_output_index(),
                "content_index": 0,
                "delta": content
            }));
        }
        if let Some(tool_calls) = delta
            .and_then(|delta| delta.get("tool_calls"))
            .and_then(Value::as_array)
        {
            for call in tool_calls {
                self.apply_tool_delta(call)?;
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            if self.finish_reason.is_some() {
                return Err(ChatProtocolError::InvalidStream);
            }
            self.reserve_aggregate_bytes(reason.len())?;
            self.finish_reason = Some(reason.to_owned());
            self.finish_items();
        }
        Ok(())
    }

    fn start_reasoning(&mut self) {
        if self.reasoning_started {
            return;
        }
        self.reasoning_started = true;
        self.emit(json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "reasoning", "id": self.reasoning_id, "summary": []}
        }));
        self.emit(json!({
            "type": "response.reasoning_summary_part.added",
            "item_id": self.reasoning_id,
            "output_index": 0,
            "summary_index": 0,
            "part": {"type": "summary_text", "text": ""}
        }));
    }

    fn start_text(&mut self) {
        if self.text_started {
            return;
        }
        self.text_started = true;
        let index = self.message_output_index();
        self.emit(json!({
            "type": "response.output_item.added",
            "output_index": index,
            "item": {"type": "message", "id": self.message_id, "role": "assistant", "content": []}
        }));
        self.emit(json!({
            "type": "response.content_part.added",
            "item_id": self.message_id,
            "output_index": index,
            "content_index": 0,
            "part": {"type": "output_text", "text": "", "annotations": []}
        }));
    }

    #[allow(clippy::assigning_clones)] // ID replacement must drop oversized retained capacity.
    fn apply_tool_delta(&mut self, call: &Value) -> Result<(), ChatProtocolError> {
        let index = call
            .get("index")
            .and_then(Value::as_u64)
            .ok_or(ChatProtocolError::InvalidStream)?;
        let new_tool = !self.tools.contains_key(&index);
        if new_tool && self.tools.len() >= MAX_STREAM_TOOL_CALLS {
            return Err(ChatProtocolError::StreamStateLimit);
        }
        let id = call.get("id").and_then(Value::as_str);
        let function = call.get("function");
        let name = function
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str);
        let arguments = function
            .and_then(|function| function.get("arguments"))
            .and_then(Value::as_str);
        let previous_id_bytes = self.tools.get(&index).map_or(0, |state| state.id.len());
        let next_id_bytes = id.map_or(previous_id_bytes, str::len);
        let next_aggregate_bytes = self
            .aggregate_bytes
            .saturating_sub(previous_id_bytes)
            .checked_add(next_id_bytes)
            .and_then(|bytes| bytes.checked_add(name.map_or(0, str::len)))
            .and_then(|bytes| bytes.checked_add(arguments.map_or(0, str::len)))
            .ok_or(ChatProtocolError::StreamStateLimit)?;
        if next_aggregate_bytes > self.max_aggregate_bytes {
            return Err(ChatProtocolError::StreamStateLimit);
        }

        let mut emit_added = None;
        let mut arguments_delta = None;
        {
            let state = self.tools.entry(index).or_default();
            if let Some(id) = id {
                state.id = id.to_owned();
            }
            if let Some(name) = name {
                state.name.push_str(name);
            }
            if let Some(arguments) = arguments {
                state.arguments.push_str(arguments);
                arguments_delta = Some(arguments.to_owned());
            }
            if !state.added && !state.id.is_empty() && !state.name.is_empty() {
                state.added = true;
                emit_added = Some((state.id.clone(), state.name.clone()));
            }
        }
        self.aggregate_bytes = next_aggregate_bytes;
        let output_index = self.tool_output_index(index);
        if let Some((id, name)) = emit_added {
            let identity = self.tool_identity(&name);
            self.emit(json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": {"type": "function_call", "id": format!("fc_{id}"), "call_id": id, "name": identity.name, "namespace": identity.namespace, "arguments": "", "status": "in_progress"}
            }));
        }
        if let Some(delta) = arguments_delta {
            let state = self.tools.get(&index).expect("tool state exists");
            self.emit(json!({
                "type": "response.function_call_arguments.delta",
                "item_id": format!("fc_{}", state.id),
                "output_index": output_index,
                "delta": delta
            }));
        }
        Ok(())
    }

    fn reserve_aggregate_bytes(&mut self, additional: usize) -> Result<(), ChatProtocolError> {
        let next = self
            .aggregate_bytes
            .checked_add(additional)
            .ok_or(ChatProtocolError::StreamStateLimit)?;
        if next > self.max_aggregate_bytes {
            return Err(ChatProtocolError::StreamStateLimit);
        }
        self.aggregate_bytes = next;
        Ok(())
    }

    fn resize_aggregate_bytes(
        &mut self,
        previous: usize,
        next: usize,
    ) -> Result<(), ChatProtocolError> {
        let resized = self
            .aggregate_bytes
            .saturating_sub(previous)
            .checked_add(next)
            .ok_or(ChatProtocolError::StreamStateLimit)?;
        if resized > self.max_aggregate_bytes {
            return Err(ChatProtocolError::StreamStateLimit);
        }
        self.aggregate_bytes = resized;
        Ok(())
    }

    fn finish_items(&mut self) {
        if self.reasoning_started {
            self.emit(json!({
                "type": "response.reasoning_summary_text.done",
                "item_id": self.reasoning_id,
                "output_index": 0,
                "summary_index": 0,
                "text": self.reasoning
            }));
            self.emit(json!({
                "type": "response.reasoning_summary_part.done",
                "item_id": self.reasoning_id,
                "output_index": 0,
                "summary_index": 0,
                "part": {"type": "summary_text", "text": self.reasoning}
            }));
            self.emit(json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "reasoning", "id": self.reasoning_id, "summary": [{"type": "summary_text", "text": self.reasoning}]}
            }));
        }
        if self.text_started {
            let index = self.message_output_index();
            self.emit(json!({
                "type": "response.output_text.done",
                "item_id": self.message_id,
                "output_index": index,
                "content_index": 0,
                "text": self.text
            }));
            self.emit(json!({
                "type": "response.content_part.done",
                "item_id": self.message_id,
                "output_index": index,
                "content_index": 0,
                "part": {"type": "output_text", "text": self.text, "annotations": []}
            }));
            self.emit(json!({
                "type": "response.output_item.done",
                "output_index": index,
                "item": {"type": "message", "id": self.message_id, "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": self.text, "annotations": []}]}
            }));
        }
        let calls = self
            .tools
            .iter()
            .map(|(index, tool)| {
                (
                    *index,
                    tool.id.clone(),
                    tool.name.clone(),
                    tool.arguments.clone(),
                )
            })
            .collect::<Vec<_>>();
        for (index, id, name, arguments) in calls {
            let output_index = self.tool_output_index(index);
            let identity = self.tool_identity(&name);
            self.emit(json!({
                "type": "response.function_call_arguments.done",
                "item_id": format!("fc_{id}"),
                "output_index": output_index,
                "arguments": arguments
            }));
            self.emit(json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": {"type": "function_call", "id": format!("fc_{id}"), "call_id": id, "name": identity.name, "namespace": identity.namespace, "arguments": arguments, "status": "completed"}
            }));
        }
    }

    fn finalize(&mut self) {
        if self.terminal {
            return;
        }
        if self.finish_reason.is_none() {
            self.finish_reason = Some("stop".to_owned());
            self.finish_items();
        }
        let response_id = self
            .response_id
            .clone()
            .unwrap_or_else(|| format!("chatcmpl_{}", random_id()));
        let output = self.response_output();
        let usage = response_usage(self.usage.as_ref());
        let reason = self.finish_reason.as_deref().unwrap_or("stop");
        let (event_type, status) = match reason {
            "stop" | "tool_calls" => {
                self.completed = true;
                ("response.completed", "completed")
            }
            "length" => ("response.incomplete", "incomplete"),
            _ => ("response.failed", "failed"),
        };
        self.emit(json!({
            "type": event_type,
            "response": {
                "id": response_id,
                "status": status,
                "output": output,
                "usage": usage
            }
        }));
        self.terminal = true;
    }

    fn response_output(&self) -> Vec<Value> {
        let mut output = Vec::new();
        if self.reasoning_started {
            output.push(json!({
                "type": "reasoning",
                "id": self.reasoning_id,
                "summary": [{"type": "summary_text", "text": self.reasoning}]
            }));
        }
        if self.text_started {
            output.push(json!({
                "type": "message",
                "id": self.message_id,
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": self.text, "annotations": []}]
            }));
        }
        output.extend(self.tools.values().map(|tool| {
            let identity = self.tool_identity(&tool.name);
            json!({
                "type": "function_call",
                "id": format!("fc_{}", tool.id),
                "call_id": tool.id,
                "name": identity.name,
                "namespace": identity.namespace,
                "arguments": tool.arguments,
                "status": "completed"
            })
        }));
        output
    }

    fn tool_identity(&self, chat_name: &str) -> ToolIdentity {
        self.tool_names
            .get(chat_name)
            .cloned()
            .unwrap_or_else(|| ToolIdentity {
                name: chat_name.to_owned(),
                namespace: None,
            })
    }

    fn message_output_index(&self) -> u64 {
        u64::from(self.reasoning_started)
    }

    fn tool_output_index(&self, tool_index: u64) -> u64 {
        u64::from(self.reasoning_started) + u64::from(self.text_started) + tool_index
    }

    #[allow(clippy::needless_pass_by_value)] // All call sites construct a one-shot JSON event.
    fn emit(&mut self, event: Value) {
        self.pending.push_back(event.to_string());
    }
}

impl WsHttpEventDecoder for ChatSseDecoder {
    type Commit = Value;

    fn push(&mut self, data: &[u8]) -> Result<Vec<String>, BridgeFailure> {
        ChatSseDecoder::push(self, data).map_err(|error| map_failure(&error))
    }

    fn finish(&mut self) -> Result<Vec<String>, BridgeFailure> {
        ChatSseDecoder::finish(self).map_err(|error| map_failure(&error))
    }

    fn is_terminal(&self) -> bool {
        ChatSseDecoder::is_terminal(self)
    }

    fn into_outcome(self) -> WsHttpStreamOutcome<Self::Commit> {
        let outcome = self.outcome();
        WsHttpStreamOutcome {
            terminal: outcome.terminal,
            completed: outcome.completed,
            commit: outcome.assistant_message,
        }
    }
}

fn map_failure(error: &ChatProtocolError) -> BridgeFailure {
    match error {
        ChatProtocolError::SessionHistoryLimit => BridgeFailure::SessionHistoryLimit,
        ChatProtocolError::InvalidStream
        | ChatProtocolError::StreamBufferLimit
        | ChatProtocolError::StreamStateLimit => BridgeFailure::InvalidStream,
        _ => BridgeFailure::InvalidRequest,
    }
}

fn response_usage(usage: Option<&Value>) -> Value {
    let input = usage
        .and_then(|usage| usage.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .and_then(|usage| usage.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = usage
        .and_then(|usage| usage.get("total_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(input.saturating_add(output));
    json!({
        "input_tokens": input,
        "input_tokens_details": null,
        "output_tokens": output,
        "output_tokens_details": null,
        "total_tokens": total
    })
}

fn normalize_newlines(buffer: &mut Vec<u8>) {
    if !buffer.contains(&b'\r') {
        return;
    }
    let mut normalized = Vec::with_capacity(buffer.len());
    let mut index = 0;
    while index < buffer.len() {
        if buffer[index] == b'\r' {
            normalized.push(b'\n');
            if buffer.get(index + 1) == Some(&b'\n') {
                index += 1;
            }
        } else {
            normalized.push(buffer[index]);
        }
        index += 1;
    }
    *buffer = normalized;
}

fn random_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}

fn warmup_events(response_id: &str) -> Vec<String> {
    [
        json!({"type": "response.created", "response": {"id": response_id}}),
        json!({
            "type": "response.completed",
            "response": {"id": response_id, "status": "completed", "output": [], "usage": response_usage(None)}
        }),
    ]
    .into_iter()
    .map(|event| event.to_string())
    .collect()
}
