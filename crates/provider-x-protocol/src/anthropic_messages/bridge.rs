use crate::output::{OutputEmitter, StreamEvent, ToolIdentity};
use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
};

use crate::{
    BridgeFailure, WsHttpAction, WsHttpEventDecoder, WsHttpProtocolAdapter, WsHttpStreamOutcome,
};
use bytes::Bytes;
use provider_x_core::AnthropicThinkingMode;
use serde_json::{Map, Value, json};

use crate::anthropic_messages::AnthropicProtocolError;

const DEFAULT_MAX_TOKENS: u64 = 8_192;
const MAX_CONTENT_BLOCKS: usize = 256;
const RAW_CONTENT_FIELD: &str = "_provider_x_anthropic_content";

pub struct AnthropicBridgeRequest {
    pub body: Bytes,
    pub messages: Vec<Value>,
    pub tool_names: BTreeMap<String, ToolIdentity>,
}

impl fmt::Debug for AnthropicBridgeRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicBridgeRequest")
            .field("body_bytes", &self.body.len())
            .field("message_count", &self.messages.len())
            .field("tool_count", &self.tool_names.len())
            .finish()
    }
}

impl AnthropicBridgeRequest {
    #[must_use]
    pub fn decoder(&self, max_buffer_bytes: usize) -> AnthropicSseDecoder {
        AnthropicSseDecoder::with_tool_names(max_buffer_bytes, self.tool_names.clone())
    }
}

pub struct AnthropicMessagesWsHttpAdapter {
    upstream_model: String,
    history: Vec<Value>,
    defaults: Map<String, Value>,
    tool_names: BTreeMap<String, ToolIdentity>,
    max_session_bytes: usize,
    thinking_mode: AnthropicThinkingMode,
}

impl AnthropicMessagesWsHttpAdapter {
    #[must_use]
    pub fn new_session_with_thinking_mode(
        upstream_model: String,
        max_session_bytes: usize,
        thinking_mode: AnthropicThinkingMode,
    ) -> Self {
        Self {
            upstream_model,
            history: Vec::new(),
            defaults: Map::new(),
            tool_names: BTreeMap::new(),
            max_session_bytes,
            thinking_mode,
        }
    }
}

impl WsHttpProtocolAdapter for AnthropicMessagesWsHttpAdapter {
    type Pending = Vec<Value>;
    type Commit = Value;
    type Decoder = AnthropicSseDecoder;
    fn new_session(upstream_model: String, max_session_bytes: usize) -> Self {
        Self::new_session_with_thinking_mode(
            upstream_model,
            max_session_bytes,
            AnthropicThinkingMode::Adaptive,
        )
    }
    fn upstream_url(http_endpoint: &str) -> String {
        crate::anthropic_messages::messages_url(http_endpoint)
    }
    fn prepare_action(&mut self, text: &str) -> Result<WsHttpAction<Self::Pending>, BridgeFailure> {
        let mut create: Value =
            serde_json::from_str(text).map_err(|_| BridgeFailure::InvalidRequest)?;
        if create["type"] != "response.create" {
            return Err(BridgeFailure::InvalidRequest);
        }
        for key in [
            "instructions",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "reasoning",
        ] {
            if let Some(value) = create.get(key) {
                self.defaults.insert(key.to_owned(), value.clone());
            }
        }
        if serde_json::to_vec(&self.defaults)
            .map_err(|_| BridgeFailure::InvalidRequest)?
            .len()
            > self.max_session_bytes
        {
            return Err(BridgeFailure::SessionHistoryLimit);
        }
        if create["generate"] == false {
            return Ok(WsHttpAction::Warmup { events: vec![
                json!({"type":"response.created","response":{"id":"resp_provider_x_warmup"}}).to_string(),
                json!({"type":"response.completed","response":{"id":"resp_provider_x_warmup","status":"completed","output":[],"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}}).to_string()
            ] });
        }
        for (key, value) in &self.defaults {
            if create.get(key).is_none() {
                create[key] = value.clone();
            }
        }
        let replay = create.get("previous_response_id").is_some()
            || create["input"].as_array().is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item["type"] == "function_call_output")
            });
        let object = create
            .as_object_mut()
            .ok_or(BridgeFailure::InvalidRequest)?;
        object.remove("previous_response_id");
        let mut request = build_request(
            object,
            &self.upstream_model,
            self.max_session_bytes,
            self.thinking_mode,
        )
        .map_err(|e| map_failure(&e))?;
        if replay && !self.history.is_empty() {
            let current = &request.messages;
            let overlap = (0..=self.history.len().min(current.len()))
                .rev()
                .find(|&n| self.history[self.history.len() - n..] == current[..n])
                .unwrap_or(0);
            let mut messages = self.history.clone();
            messages.extend_from_slice(&current[overlap..]);
            validate_history(&messages, self.max_session_bytes).map_err(|e| map_failure(&e))?;
            let mut body: Value =
                serde_json::from_slice(&request.body).map_err(|_| BridgeFailure::InvalidRequest)?;
            body["messages"] = Value::Array(messages.clone());
            request.body =
                Bytes::from(serde_json::to_vec(&body).map_err(|_| BridgeFailure::InvalidRequest)?);
            if request.body.len() > self.max_session_bytes {
                return Err(BridgeFailure::SessionHistoryLimit);
            }
            request.messages = messages;
        }
        self.tool_names.clone_from(&request.tool_names);
        Ok(WsHttpAction::Request {
            body: request.body,
            pending: request.messages,
        })
    }
    fn new_decoder(&self, max_buffer_bytes: usize) -> Self::Decoder {
        AnthropicSseDecoder::with_tool_names(max_buffer_bytes, self.tool_names.clone())
    }
    fn commit_outcome(
        &mut self,
        mut pending: Vec<Value>,
        commit: Value,
    ) -> Result<(), BridgeFailure> {
        let content = commit
            .get(RAW_CONTENT_FIELD)
            .cloned()
            .ok_or(BridgeFailure::InvalidStream)?;
        pending.push(json!({"role":"assistant","content":content}));
        validate_history(&pending, self.max_session_bytes).map_err(|e| map_failure(&e))?;
        self.history = pending;
        Ok(())
    }
}

fn validate_history(messages: &[Value], limit: usize) -> Result<(), AnthropicProtocolError> {
    if messages.len() > 256
        || serde_json::to_vec(messages)
            .map_err(|_| AnthropicProtocolError::InvalidRequest)?
            .len()
            > limit
    {
        return Err(AnthropicProtocolError::SessionHistoryLimit);
    }
    Ok(())
}

/// Converts an HTTP Responses request body to an Anthropic Messages request.
///
/// # Errors
///
/// Returns an error for malformed or unsupported Responses input, or when the converted request
/// exceeds `max_bytes`.
pub fn prepare_http_request(
    body: &[u8],
    upstream_model: &str,
    max_bytes: usize,
) -> Result<AnthropicBridgeRequest, AnthropicProtocolError> {
    prepare_http_request_with_thinking_mode(
        body,
        upstream_model,
        max_bytes,
        AnthropicThinkingMode::Adaptive,
    )
}

/// Converts an HTTP Responses request body using the configured Anthropic thinking strategy.
///
/// # Errors
///
/// Returns an error for malformed or unsupported Responses input, or when the converted request
/// exceeds `max_bytes`.
pub fn prepare_http_request_with_thinking_mode(
    body: &[u8],
    upstream_model: &str,
    max_bytes: usize,
    thinking_mode: AnthropicThinkingMode,
) -> Result<AnthropicBridgeRequest, AnthropicProtocolError> {
    let create: Value =
        serde_json::from_slice(body).map_err(|_| AnthropicProtocolError::InvalidRequest)?;
    let create = create
        .as_object()
        .ok_or(AnthropicProtocolError::InvalidRequest)?;
    if create.contains_key("previous_response_id") {
        return Err(AnthropicProtocolError::UnsupportedInput(
            "previous_response_id requires a WebSocket session".to_owned(),
        ));
    }
    build_request(create, upstream_model, max_bytes, thinking_mode)
}

fn build_request(
    create: &Map<String, Value>,
    upstream_model: &str,
    max_bytes: usize,
    thinking_mode: AnthropicThinkingMode,
) -> Result<AnthropicBridgeRequest, AnthropicProtocolError> {
    let (messages, mut system) = super::input::messages(create.get("input"))?;
    validate_history(&messages, max_bytes)?;
    if let Some(instructions) = create.get("instructions").and_then(Value::as_str) {
        system.insert(0, instructions.to_owned());
    }
    let mut request = Map::new();
    request.insert("model".to_owned(), Value::String(upstream_model.to_owned()));
    request.insert("messages".to_owned(), Value::Array(messages.clone()));
    request.insert("stream".to_owned(), Value::Bool(true));
    if !system.is_empty() {
        request.insert("system".to_owned(), Value::String(system.join("\n\n")));
    }
    let max_tokens = create
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    request.insert("max_tokens".to_owned(), Value::from(max_tokens));
    let (tools, tool_names) = super::input::tools(create.get("tools"))?;
    let has_tools = !tools.is_empty();
    if has_tools {
        request.insert("tools".to_owned(), Value::Array(tools));
    }
    let reasoning_effort = create
        .get("reasoning")
        .and_then(|v| v.get("effort"))
        .and_then(Value::as_str)
        .map(|v| {
            match v {
                "xhigh" => "max",
                other => other,
            }
            .to_owned()
        });
    let parallel_tool_calls = create.get("parallel_tool_calls").and_then(Value::as_bool);
    let tool_choice = if let Some(choice) = create.get("tool_choice").filter(|_| has_tools) {
        super::input::tool_choice(choice, parallel_tool_calls, &tool_names)?
    } else if has_tools && parallel_tool_calls == Some(false) {
        Some(json!({"type":"auto", "disable_parallel_tool_use":true}))
    } else {
        None
    };
    let forced_tool = tool_choice.as_ref().is_some_and(|choice| {
        matches!(
            choice.get("type").and_then(Value::as_str),
            Some("any" | "tool")
        )
    });
    if let Some(mut choice) = tool_choice {
        if forced_tool && thinking_mode == AnthropicThinkingMode::Enabled {
            append_forced_tool_instruction(&mut request, &choice)?;
            let disable_parallel = choice
                .get("disable_parallel_tool_use")
                .and_then(Value::as_bool);
            choice = json!({"type":"auto"});
            if let Some(disable_parallel) = disable_parallel {
                choice["disable_parallel_tool_use"] = Value::Bool(disable_parallel);
            }
        }
        request.insert("tool_choice".to_owned(), choice);
    }
    if let Some(effort) = reasoning_effort {
        request.insert("output_config".to_owned(), json!({"effort": effort}));
        match thinking_mode {
            AnthropicThinkingMode::Adaptive => {
                request.insert("thinking".to_owned(), json!({"type":"adaptive"}));
            }
            AnthropicThinkingMode::Enabled => {
                if max_tokens <= 1_024 {
                    return Err(AnthropicProtocolError::UnsupportedInput(
                        "enabled Anthropic thinking requires max_output_tokens above 1024"
                            .to_owned(),
                    ));
                }
                let desired = match effort.as_str() {
                    "low" => 1_024,
                    "medium" => 2_048,
                    "high" => 4_096,
                    _ => 8_192,
                };
                request.insert(
                    "thinking".to_owned(),
                    json!({"type":"enabled", "budget_tokens": desired.min(max_tokens - 1)}),
                );
            }
        }
    }
    let bytes = serde_json::to_vec(&Value::Object(request))
        .map_err(|error| AnthropicProtocolError::Serialization(error.to_string()))?;
    if bytes.len() > max_bytes {
        return Err(AnthropicProtocolError::SessionHistoryLimit);
    }
    Ok(AnthropicBridgeRequest {
        body: Bytes::from(bytes),
        messages,
        tool_names,
    })
}

fn append_forced_tool_instruction(
    request: &mut Map<String, Value>,
    choice: &Value,
) -> Result<(), AnthropicProtocolError> {
    let instruction = match choice.get("type").and_then(Value::as_str) {
        Some("tool") => format!(
            "You must call the {} tool in this turn before responding.",
            choice
                .get("name")
                .and_then(Value::as_str)
                .ok_or(AnthropicProtocolError::InvalidRequest)?
        ),
        Some("any") => {
            "You must call one available tool in this turn before responding.".to_owned()
        }
        _ => return Ok(()),
    };
    match request.entry("system") {
        serde_json::map::Entry::Vacant(entry) => {
            entry.insert(Value::String(instruction));
        }
        serde_json::map::Entry::Occupied(mut entry) => {
            let system = entry
                .get_mut()
                .as_str()
                .ok_or(AnthropicProtocolError::InvalidRequest)?
                .to_owned();
            entry.insert(Value::String(format!("{system}\n\n{instruction}")));
        }
    }
    Ok(())
}

pub struct AnthropicSseDecoder {
    buffer: Vec<u8>,
    max_buffer_bytes: usize,
    inner: OutputEmitter,
    content: BTreeMap<u64, Value>,
    tool_json: BTreeMap<u64, String>,
    tool_indices: BTreeMap<u64, u64>,
    pending: VecDeque<String>,
    input_tokens: u64,
    output_tokens: u64,
    aggregate_bytes: usize,
}

pub struct AnthropicStreamOutcome {
    pub terminal: bool,
    pub completed: bool,
    pub assistant_message: Value,
}

impl AnthropicSseDecoder {
    #[must_use]
    pub fn new(max_buffer_bytes: usize) -> Self {
        Self::with_tool_names(max_buffer_bytes, BTreeMap::new())
    }

    #[must_use]
    pub fn with_tool_names(
        max_buffer_bytes: usize,
        tool_names: BTreeMap<String, ToolIdentity>,
    ) -> Self {
        Self {
            buffer: Vec::new(),
            max_buffer_bytes,
            inner: OutputEmitter::with_tool_names(max_buffer_bytes, tool_names),
            content: BTreeMap::new(),
            tool_json: BTreeMap::new(),
            tool_indices: BTreeMap::new(),
            pending: VecDeque::new(),
            input_tokens: 0,
            output_tokens: 0,
            aggregate_bytes: 0,
        }
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.inner.is_terminal()
    }

    /// Converts arbitrary Anthropic Messages SSE chunks to Responses WebSocket events.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed JSON, event-buffer overflow, or aggregate-state overflow.
    pub fn push(&mut self, data: &[u8]) -> Result<Vec<String>, AnthropicProtocolError> {
        if self.buffer.len().saturating_add(data.len()) > self.max_buffer_bytes {
            return Err(AnthropicProtocolError::StreamBufferLimit);
        }
        self.buffer.extend_from_slice(data);
        normalize_newlines(&mut self.buffer);
        while let Some(index) = self.buffer.windows(2).position(|window| window == b"\n\n") {
            let block: Vec<_> = self.buffer.drain(..index + 2).collect();
            self.parse_block(&block)?;
        }
        Ok(self.pending.drain(..).collect())
    }

    /// Flushes a final unterminated Anthropic SSE block.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed stream data.
    pub fn finish(&mut self) -> Result<Vec<String>, AnthropicProtocolError> {
        if !self.buffer.is_empty() {
            let block = std::mem::take(&mut self.buffer);
            self.parse_block(&block)?;
        }
        if !self.inner.is_terminal() {
            return Err(AnthropicProtocolError::InvalidStream);
        }
        self.pending.extend(self.inner.drain());
        Ok(self.pending.drain(..).collect())
    }

    #[must_use]
    pub fn outcome(self) -> AnthropicStreamOutcome {
        let content = self.content.into_values().collect::<Vec<_>>();
        let outcome = self.inner.outcome();
        let mut assistant_message = outcome.assistant_message;
        assistant_message[RAW_CONTENT_FIELD] = Value::Array(content);
        AnthropicStreamOutcome {
            terminal: outcome.terminal,
            completed: outcome.completed,
            assistant_message,
        }
    }

    fn parse_block(&mut self, block: &[u8]) -> Result<(), AnthropicProtocolError> {
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
        let value: Value =
            serde_json::from_str(&data).map_err(|_| AnthropicProtocolError::InvalidStream)?;
        self.apply_event(&value)?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)] // Keeps the bounded Anthropic SSE state machine in event order.
    fn apply_event(&mut self, event: &Value) -> Result<(), AnthropicProtocolError> {
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let message = event
                    .get("message")
                    .ok_or(AnthropicProtocolError::InvalidStream)?;
                let id = message
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("message");
                self.input_tokens = message
                    .get("usage")
                    .and_then(|usage| usage.get("input_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                self.feed(StreamEvent::Start(id))?;
                self.feed(StreamEvent::Usage {
                    input: self.input_tokens,
                    output: self.output_tokens,
                })?;
            }
            Some("content_block_start") => {
                let index = content_index(event)?;
                if !self.content.contains_key(&index) && self.content.len() >= MAX_CONTENT_BLOCKS {
                    return Err(AnthropicProtocolError::StreamStateLimit);
                }
                let block = event
                    .get("content_block")
                    .cloned()
                    .ok_or(AnthropicProtocolError::InvalidStream)?;
                self.reserve_aggregate_bytes(block.to_string().len())?;
                self.content.insert(index, block.clone());
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            self.feed(StreamEvent::Text(text))?;
                        }
                    }
                    Some("thinking") => {
                        if let Some(thinking) = block.get("thinking").and_then(Value::as_str)
                            && !thinking.is_empty()
                        {
                            self.feed(StreamEvent::Reasoning(thinking))?;
                        }
                    }
                    Some("tool_use") => {
                        let tool_index = u64::try_from(self.tool_indices.len())
                            .map_err(|_| AnthropicProtocolError::StreamStateLimit)?;
                        let tool_index = *self.tool_indices.entry(index).or_insert(tool_index);
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .ok_or(AnthropicProtocolError::InvalidStream)?;
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .ok_or(AnthropicProtocolError::InvalidStream)?;
                        let arguments = block
                            .get("input")
                            .filter(|input| {
                                input.as_object().is_some_and(|input| !input.is_empty())
                            })
                            .map(Value::to_string)
                            .unwrap_or_default();
                        self.feed(StreamEvent::Tool {
                            index: tool_index,
                            id: Some(id),
                            name: Some(name),
                            arguments: Some(&arguments),
                        })?;
                    }
                    Some("redacted_thinking") => {}
                    _ => return Err(AnthropicProtocolError::InvalidStream),
                }
            }
            Some("content_block_delta") => {
                let index = content_index(event)?;
                let delta = event
                    .get("delta")
                    .ok_or(AnthropicProtocolError::InvalidStream)?;
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        let text = delta
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        self.reserve_aggregate_bytes(text.len())?;
                        append_block_text(&mut self.content, index, "text", text)?;
                        self.feed(StreamEvent::Text(text))?;
                    }
                    Some("thinking_delta") => {
                        let thinking = delta
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        self.reserve_aggregate_bytes(thinking.len())?;
                        append_block_text(&mut self.content, index, "thinking", thinking)?;
                        self.feed(StreamEvent::Reasoning(thinking))?;
                    }
                    Some("signature_delta") => {
                        let signature = delta
                            .get("signature")
                            .and_then(Value::as_str)
                            .ok_or(AnthropicProtocolError::InvalidStream)?;
                        self.reserve_aggregate_bytes(signature.len())?;
                        let block = self
                            .content
                            .get_mut(&index)
                            .and_then(Value::as_object_mut)
                            .ok_or(AnthropicProtocolError::InvalidStream)?;
                        match block.entry("signature") {
                            serde_json::map::Entry::Vacant(entry) => {
                                entry.insert(Value::String(signature.to_owned()));
                            }
                            serde_json::map::Entry::Occupied(mut entry) => {
                                let Value::String(current) = entry.get_mut() else {
                                    return Err(AnthropicProtocolError::InvalidStream);
                                };
                                current.push_str(signature);
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        let tool_index = self
                            .tool_indices
                            .get(&index)
                            .copied()
                            .ok_or(AnthropicProtocolError::InvalidStream)?;
                        let partial = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        self.reserve_aggregate_bytes(partial.len())?;
                        self.tool_json.entry(index).or_default().push_str(partial);
                        self.feed(StreamEvent::Tool {
                            index: tool_index,
                            id: None,
                            name: None,
                            arguments: Some(partial),
                        })?;
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                let index = content_index(event)?;
                if let Some(input) = self.tool_json.remove(&index) {
                    let input = serde_json::from_str(&input)
                        .map_err(|_| AnthropicProtocolError::InvalidStream)?;
                    self.content
                        .get_mut(&index)
                        .and_then(Value::as_object_mut)
                        .ok_or(AnthropicProtocolError::InvalidStream)?
                        .insert("input".to_owned(), input);
                }
            }
            Some("message_delta") => {
                let reason = event
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                    .map(|reason| match reason {
                        "end_turn" | "stop_sequence" => "stop",
                        "tool_use" => "tool_calls",
                        "max_tokens" => "length",
                        _ => "error",
                    });
                if let Some(reason) = reason {
                    self.feed(StreamEvent::Stop(reason))?;
                }
                if let Some(usage) = event.get("usage") {
                    self.output_tokens = usage
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(self.output_tokens);
                    self.feed(StreamEvent::Usage {
                        input: self.input_tokens,
                        output: self.output_tokens,
                    })?;
                }
            }
            Some("message_stop") => self.feed(StreamEvent::Done)?,
            Some("error") | None => return Err(AnthropicProtocolError::InvalidStream),
            Some(_) => {}
        }
        Ok(())
    }

    fn feed(&mut self, event: StreamEvent<'_>) -> Result<(), AnthropicProtocolError> {
        self.inner.apply(event).map_err(|error| match error {
            crate::output::OutputError::InvalidStream => AnthropicProtocolError::InvalidStream,
            crate::output::OutputError::StreamStateLimit => {
                AnthropicProtocolError::StreamStateLimit
            }
        })?;
        self.pending.extend(self.inner.drain());
        Ok(())
    }

    fn reserve_aggregate_bytes(&mut self, additional: usize) -> Result<(), AnthropicProtocolError> {
        let next = self
            .aggregate_bytes
            .checked_add(additional)
            .ok_or(AnthropicProtocolError::StreamStateLimit)?;
        if next > self.max_buffer_bytes {
            return Err(AnthropicProtocolError::StreamStateLimit);
        }
        self.aggregate_bytes = next;
        Ok(())
    }
}

impl WsHttpEventDecoder for AnthropicSseDecoder {
    type Commit = Value;

    fn push(&mut self, data: &[u8]) -> Result<Vec<String>, BridgeFailure> {
        AnthropicSseDecoder::push(self, data).map_err(|error| map_failure(&error))
    }

    fn finish(&mut self) -> Result<Vec<String>, BridgeFailure> {
        AnthropicSseDecoder::finish(self).map_err(|error| map_failure(&error))
    }

    fn is_terminal(&self) -> bool {
        AnthropicSseDecoder::is_terminal(self)
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

fn append_block_text(
    content: &mut BTreeMap<u64, Value>,
    index: u64,
    field: &str,
    delta: &str,
) -> Result<(), AnthropicProtocolError> {
    let value = content
        .get_mut(&index)
        .and_then(|block| block.get_mut(field))
        .ok_or(AnthropicProtocolError::InvalidStream)?;
    let Value::String(text) = value else {
        return Err(AnthropicProtocolError::InvalidStream);
    };
    text.push_str(delta);
    Ok(())
}

fn content_index(event: &Value) -> Result<u64, AnthropicProtocolError> {
    event
        .get("index")
        .and_then(Value::as_u64)
        .ok_or(AnthropicProtocolError::InvalidStream)
}

fn normalize_newlines(buffer: &mut Vec<u8>) {
    let mut read = 0;
    let mut write = 0;
    while read < buffer.len() {
        if buffer[read] == b'\r' {
            buffer[write] = b'\n';
            write += 1;
            read += usize::from(buffer.get(read + 1) == Some(&b'\n')) + 1;
        } else {
            buffer[write] = buffer[read];
            write += 1;
            read += 1;
        }
    }
    buffer.truncate(write);
}

fn map_failure(error: &AnthropicProtocolError) -> BridgeFailure {
    match error {
        AnthropicProtocolError::SessionHistoryLimit => BridgeFailure::SessionHistoryLimit,
        AnthropicProtocolError::InvalidStream
        | AnthropicProtocolError::StreamBufferLimit
        | AnthropicProtocolError::StreamStateLimit => BridgeFailure::InvalidStream,

        _ => BridgeFailure::InvalidRequest,
    }
}
