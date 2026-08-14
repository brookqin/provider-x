use std::collections::HashMap;

use bytes::Bytes;
use provider_x_core::TransportConfig;
use provider_x_protocol::{
    BridgeFailure, WsHttpAction, WsHttpEventDecoder, WsHttpProtocolAdapter, WsHttpStreamOutcome,
};
use serde_json::{Map, Value};

use crate::{ProtocolError, inspect_ws_text};

const MAX_HISTORY_ITEMS: usize = 256;
const MAX_REASONING_ITEMS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesWebSocketPlan {
    DirectWebSocket,
    BridgeToHttpSse,
}

#[must_use]
pub fn websocket_ingress_plan(transports: &TransportConfig) -> Option<ResponsesWebSocketPlan> {
    if transports.websocket {
        Some(ResponsesWebSocketPlan::DirectWebSocket)
    } else if transports.http_sse {
        Some(ResponsesWebSocketPlan::BridgeToHttpSse)
    } else {
        None
    }
}

#[derive(Default)]
struct WarmupDefaults {
    instructions: Option<Value>,
    tools: Option<Value>,
    parallel_tool_calls: Option<Value>,
    tool_choice: Option<Value>,
}

pub struct WsHttpBridgeSession {
    upstream_model: String,
    warmup_response_id: Option<String>,
    warmup: WarmupDefaults,
    history: Vec<Value>,
    max_history_bytes: usize,
}

pub enum BridgeAction {
    Warmup { events: Vec<String> },
    Request(BridgeRequest),
}

pub struct BridgeRequest {
    pub body: Bytes,
    pub input: Vec<Value>,
}

pub struct ResponsesWsHttpAdapter {
    session: WsHttpBridgeSession,
}

impl WsHttpProtocolAdapter for ResponsesWsHttpAdapter {
    type Pending = Vec<Value>;
    type Commit = Vec<Value>;
    type Decoder = SseStreamDecoder;

    fn new_session(upstream_model: String, max_session_bytes: usize) -> Self {
        Self {
            session: WsHttpBridgeSession::new(upstream_model, max_session_bytes),
        }
    }

    fn upstream_url(http_endpoint: &str) -> String {
        crate::responses_url(http_endpoint)
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
            BridgeAction::Warmup { events } => Ok(WsHttpAction::Warmup { events }),
            BridgeAction::Request(request) => Ok(WsHttpAction::Request {
                body: request.body,
                pending: request.input,
            }),
        }
    }

    fn new_decoder(&self, max_buffer_bytes: usize) -> Self::Decoder {
        SseStreamDecoder::new(max_buffer_bytes)
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

impl WsHttpBridgeSession {
    #[must_use]
    pub fn new(upstream_model: String, max_history_bytes: usize) -> Self {
        Self {
            upstream_model,
            warmup_response_id: None,
            warmup: WarmupDefaults::default(),
            history: Vec::new(),
            max_history_bytes,
        }
    }

    /// Converts one WebSocket `response.create` into either a local warmup or an HTTP request.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid Responses JSON or when bounded history limits are exceeded.
    pub fn prepare(&mut self, text: &str) -> Result<BridgeAction, ProtocolError> {
        let mut create = parse_response_create(text)?;
        let generate = create.get("generate").and_then(Value::as_bool) != Some(false);
        if !generate {
            self.capture_warmup(&create);
            let response_id = format!("resp_provider_x_warmup_{}", random_id());
            self.warmup_response_id = Some(response_id.clone());
            return Ok(BridgeAction::Warmup {
                events: warmup_events(&response_id),
            });
        }

        self.capture_explicit_defaults(&create);
        self.apply_warmup_defaults(&mut create);
        create.remove("type");
        create.remove("generate");
        create.insert(
            "model".to_owned(),
            Value::String(self.upstream_model.clone()),
        );
        create.insert("stream".to_owned(), Value::Bool(true));

        let previous_response_id = create
            .get("previous_response_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let references_warmup = previous_response_id
            .as_deref()
            .zip(self.warmup_response_id.as_deref())
            .is_some_and(|(previous, warmup)| previous == warmup);
        let current_input = take_input_items(create.remove("input"));
        let replay_history = !self.history.is_empty()
            && (previous_response_id.is_some() || contains_tool_output(&current_input));
        if references_warmup || replay_history {
            create.remove("previous_response_id");
        }

        let input = if replay_history {
            self.bounded_merge(&current_input)?
        } else {
            current_input
        };
        create.insert("input".to_owned(), Value::Array(input.clone()));
        self.warmup_response_id = None;

        let body = serde_json::to_vec(&Value::Object(create))
            .map(Bytes::from)
            .map_err(|error| ProtocolError::Serialization(error.to_string()))?;
        if body.len() > self.max_history_bytes {
            return Err(ProtocolError::SessionHistoryLimit);
        }
        Ok(BridgeAction::Request(BridgeRequest { body, input }))
    }

    /// Commits a successfully completed upstream response for a later tool continuation.
    ///
    /// # Errors
    ///
    /// Returns an error when the resulting history exceeds the item or byte limit.
    pub fn commit(&mut self, input: Vec<Value>, output: Vec<Value>) -> Result<(), ProtocolError> {
        let history: Vec<_> = input.into_iter().chain(output).collect();
        let history_bytes = serialized_len(&history)?;
        if history.len() > MAX_HISTORY_ITEMS || history_bytes > self.max_history_bytes {
            return Err(ProtocolError::SessionHistoryLimit);
        }
        self.history = history;
        Ok(())
    }

    fn capture_warmup(&mut self, create: &Map<String, Value>) {
        self.warmup.instructions = create.get("instructions").cloned();
        self.warmup.tools = create.get("tools").cloned();
        self.warmup.parallel_tool_calls = create.get("parallel_tool_calls").cloned();
        self.warmup.tool_choice = create.get("tool_choice").cloned();
    }

    fn capture_explicit_defaults(&mut self, create: &Map<String, Value>) {
        for (name, target) in [
            ("instructions", &mut self.warmup.instructions),
            ("tools", &mut self.warmup.tools),
            ("parallel_tool_calls", &mut self.warmup.parallel_tool_calls),
            ("tool_choice", &mut self.warmup.tool_choice),
        ] {
            if let Some(value) = create.get(name) {
                *target = Some(value.clone());
            }
        }
    }

    fn apply_warmup_defaults(&self, create: &mut Map<String, Value>) {
        for (name, value) in [
            ("instructions", self.warmup.instructions.as_ref()),
            ("tools", self.warmup.tools.as_ref()),
            (
                "parallel_tool_calls",
                self.warmup.parallel_tool_calls.as_ref(),
            ),
            ("tool_choice", self.warmup.tool_choice.as_ref()),
        ] {
            if !create.contains_key(name)
                && let Some(value) = value
            {
                create.insert(name.to_owned(), value.clone());
            }
        }
    }

    fn bounded_merge(&self, current: &[Value]) -> Result<Vec<Value>, ProtocolError> {
        let mut merged = self.history.clone();
        let overlap = (0..=self.history.len().min(current.len()))
            .rev()
            .find(|&count| {
                self.history[self.history.len() - count..]
                    .iter()
                    .zip(&current[..count])
                    .all(|(history, current)| same_item(history, current))
            })
            .unwrap_or(0);
        merged.extend_from_slice(&current[overlap..]);
        if merged.len() > MAX_HISTORY_ITEMS || serialized_len(&merged)? > self.max_history_bytes {
            return Err(ProtocolError::SessionHistoryLimit);
        }
        Ok(merged)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReasoningStreamSource {
    NativeSummary,
    RawAsSummary,
}

struct ReasoningCompatibility {
    sources: HashMap<String, ReasoningStreamSource>,
    item_id_bytes: usize,
    max_item_id_bytes: usize,
}

pub struct SseStreamDecoder {
    buffer: Vec<u8>,
    max_buffer_bytes: usize,
    terminal: bool,
    completed: bool,
    output: Vec<Value>,
    reasoning: ReasoningCompatibility,
}

impl Default for SseStreamDecoder {
    fn default() -> Self {
        Self::new(usize::MAX)
    }
}

pub struct SseStreamOutcome {
    pub terminal: bool,
    pub completed: bool,
    pub output: Vec<Value>,
}

impl SseStreamDecoder {
    #[must_use]
    pub fn new(max_buffer_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_buffer_bytes,
            terminal: false,
            completed: false,
            output: Vec::new(),
            reasoning: ReasoningCompatibility::new(max_buffer_bytes),
        }
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Adds arbitrary HTTP body bytes and returns complete WebSocket JSON event payloads.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid Responses JSON, event-buffer overflow, or reasoning-state
    /// overflow.
    pub fn push(&mut self, data: &[u8]) -> Result<Vec<String>, ProtocolError> {
        if self.buffer.len().saturating_add(data.len()) > self.max_buffer_bytes {
            return Err(ProtocolError::StreamBufferLimit);
        }
        self.buffer.extend_from_slice(data);
        normalize_sse_newlines(&mut self.buffer);
        let mut events = Vec::new();
        while let Some(index) = self.buffer.windows(2).position(|window| window == b"\n\n") {
            let block: Vec<_> = self.buffer.drain(..index + 2).collect();
            if let Some(event) = self.parse_block(&block)? {
                events.push(event);
            }
        }
        Ok(events)
    }

    /// Flushes the final unterminated SSE block, if any.
    ///
    /// # Errors
    ///
    /// Returns an error if the final block is not valid Responses JSON.
    pub fn finish(&mut self) -> Result<Vec<String>, ProtocolError> {
        if self.buffer.is_empty() {
            return Ok(Vec::new());
        }
        let block = std::mem::take(&mut self.buffer);
        Ok(self.parse_block(&block)?.into_iter().collect())
    }

    #[must_use]
    pub fn outcome(self) -> SseStreamOutcome {
        SseStreamOutcome {
            terminal: self.terminal,
            completed: self.completed,
            output: self.output,
        }
    }

    fn parse_block(&mut self, block: &[u8]) -> Result<Option<String>, ProtocolError> {
        let block = String::from_utf8_lossy(block).replace("\r\n", "\n");
        let data = block
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() || data == "[DONE]" {
            return Ok(None);
        }
        let value: Value = serde_json::from_str(&data).map_err(|_| ProtocolError::InvalidStream)?;
        if matches!(
            value.get("type").and_then(Value::as_str),
            Some("response.completed" | "response.failed" | "response.incomplete")
        ) {
            self.terminal = true;
        }
        if value.get("type").and_then(Value::as_str) == Some("response.completed") {
            self.completed = true;
            self.output = value
                .pointer("/response/output")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
        }
        self.reasoning
            .normalize(value)?
            .map(|event| serde_json::to_string(&event))
            .transpose()
            .map_err(|_| ProtocolError::InvalidStream)
    }
}

impl WsHttpEventDecoder for SseStreamDecoder {
    type Commit = Vec<Value>;

    fn push(&mut self, data: &[u8]) -> Result<Vec<String>, BridgeFailure> {
        SseStreamDecoder::push(self, data).map_err(|error| map_failure(&error))
    }

    fn finish(&mut self) -> Result<Vec<String>, BridgeFailure> {
        SseStreamDecoder::finish(self).map_err(|error| map_failure(&error))
    }

    fn is_terminal(&self) -> bool {
        SseStreamDecoder::is_terminal(self)
    }

    fn into_outcome(self) -> WsHttpStreamOutcome<Self::Commit> {
        let outcome = self.outcome();
        WsHttpStreamOutcome {
            terminal: outcome.terminal,
            completed: outcome.completed,
            commit: outcome.output,
        }
    }
}

fn map_failure(error: &ProtocolError) -> BridgeFailure {
    match error {
        ProtocolError::SessionHistoryLimit => BridgeFailure::SessionHistoryLimit,
        ProtocolError::InvalidStream
        | ProtocolError::StreamBufferLimit
        | ProtocolError::StreamStateLimit => BridgeFailure::InvalidStream,
        _ => BridgeFailure::InvalidRequest,
    }
}

impl ReasoningCompatibility {
    fn new(max_item_id_bytes: usize) -> Self {
        Self {
            sources: HashMap::new(),
            item_id_bytes: 0,
            max_item_id_bytes,
        }
    }

    fn normalize(&mut self, mut event: Value) -> Result<Option<Value>, ProtocolError> {
        let Some(event_type) = event.get("type").and_then(Value::as_str).map(str::to_owned) else {
            return Ok(None);
        };
        let item_id = event
            .get("item_id")
            .and_then(Value::as_str)
            .map(str::to_owned);

        if is_native_reasoning_summary_event(&event_type) {
            if let Some(item_id) = item_id {
                if self.record_source(&item_id, ReasoningStreamSource::NativeSummary)?
                    == Some(ReasoningStreamSource::RawAsSummary)
                {
                    return Ok(None);
                }
            }
            return Ok(Some(event));
        }

        if is_raw_reasoning_event(&event) {
            if let Some(item_id) = item_id {
                if self.record_source(&item_id, ReasoningStreamSource::RawAsSummary)?
                    == Some(ReasoningStreamSource::NativeSummary)
                {
                    return Ok(None);
                }
            }
            normalize_raw_reasoning_event(&mut event, &event_type);
            return Ok(Some(event));
        }

        match event_type.as_str() {
            "response.output_item.done" => {
                let completed_item_id = event
                    .pointer("/item/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Some(item) = event.get_mut("item") {
                    normalize_reasoning_item(item);
                }
                if let Some(item_id) = completed_item_id {
                    self.remove_source(&item_id);
                }
            }
            "response.completed" => {
                if let Some(output) = event
                    .pointer_mut("/response/output")
                    .and_then(Value::as_array_mut)
                {
                    output.iter_mut().for_each(normalize_reasoning_item);
                }
            }
            _ => {}
        }
        Ok(Some(event))
    }

    fn record_source(
        &mut self,
        item_id: &str,
        source: ReasoningStreamSource,
    ) -> Result<Option<ReasoningStreamSource>, ProtocolError> {
        if let Some(existing) = self.sources.get(item_id) {
            return Ok(Some(*existing));
        }
        let next_item_id_bytes = self
            .item_id_bytes
            .checked_add(item_id.len())
            .ok_or(ProtocolError::StreamStateLimit)?;
        if self.sources.len() >= MAX_REASONING_ITEMS || next_item_id_bytes > self.max_item_id_bytes
        {
            return Err(ProtocolError::StreamStateLimit);
        }
        self.sources.insert(item_id.to_owned(), source);
        self.item_id_bytes = next_item_id_bytes;
        Ok(None)
    }

    fn remove_source(&mut self, item_id: &str) {
        if let Some((stored_id, _)) = self.sources.remove_entry(item_id) {
            self.item_id_bytes = self.item_id_bytes.saturating_sub(stored_id.len());
        }
    }
}

fn parse_response_create(text: &str) -> Result<Map<String, Value>, ProtocolError> {
    inspect_ws_text(text)?;
    let value: Value = serde_json::from_str(text)
        .map_err(|error| ProtocolError::InvalidJson(error.to_string()))?;
    let object = value
        .as_object()
        .cloned()
        .ok_or(ProtocolError::BodyMustBeObject)?;
    if object.get("type").and_then(Value::as_str) != Some("response.create") {
        return Err(ProtocolError::UnsupportedWebSocketMessage);
    }
    Ok(object)
}

fn take_input_items(input: Option<Value>) -> Vec<Value> {
    match input {
        Some(Value::Array(items)) => items,
        Some(Value::Null) | None => Vec::new(),
        Some(value) => vec![value],
    }
}

fn serialized_len(values: &[Value]) -> Result<usize, ProtocolError> {
    serde_json::to_vec(values)
        .map(|bytes| bytes.len())
        .map_err(|error| ProtocolError::Serialization(error.to_string()))
}

fn contains_tool_output(input: &[Value]) -> bool {
    input.iter().any(|item| {
        matches!(
            item.get("type").and_then(Value::as_str),
            Some(
                "function_call_output"
                    | "custom_tool_call_output"
                    | "computer_call_output"
                    | "mcp_call_output"
            )
        )
    })
}

fn item_identity(item: &Value) -> Option<(&str, &str)> {
    let item_type = item.get("type")?.as_str()?;
    let id = item.get("id").or_else(|| item.get("call_id"))?.as_str()?;
    Some((item_type, id))
}

fn same_item(left: &Value, right: &Value) -> bool {
    match (item_identity(left), item_identity(right)) {
        (Some(left), Some(right)) => left == right,
        _ => left == right,
    }
}

fn random_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:x}")
}

fn warmup_events(response_id: &str) -> Vec<String> {
    [
        serde_json::json!({"type": "response.created", "response": {"id": response_id}}),
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "output": [],
                "usage": {
                    "input_tokens": 0,
                    "input_tokens_details": null,
                    "output_tokens": 0,
                    "output_tokens_details": null,
                    "total_tokens": 0
                }
            }
        }),
    ]
    .into_iter()
    .map(|event| event.to_string())
    .collect()
}

fn normalize_sse_newlines(buffer: &mut Vec<u8>) {
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

fn is_native_reasoning_summary_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
    )
}

fn is_raw_reasoning_event(event: &Value) -> bool {
    match event.get("type").and_then(Value::as_str) {
        Some("response.reasoning_text.delta" | "response.reasoning_text.done") => true,
        Some("response.content_part.added" | "response.content_part.done") => {
            event
                .get("part")
                .and_then(|part| part.get("type"))
                .and_then(Value::as_str)
                == Some("reasoning_text")
        }
        _ => false,
    }
}

fn normalize_raw_reasoning_event(event: &mut Value, event_type: &str) {
    let normalized_type = match event_type {
        "response.content_part.added" => "response.reasoning_summary_part.added",
        "response.content_part.done" => "response.reasoning_summary_part.done",
        "response.reasoning_text.delta" => "response.reasoning_summary_text.delta",
        "response.reasoning_text.done" => "response.reasoning_summary_text.done",
        _ => return,
    };
    event["type"] = Value::String(normalized_type.to_owned());
    let content_index = event
        .get("content_index")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    event["summary_index"] = Value::from(content_index);
    event
        .as_object_mut()
        .expect("SSE event must be an object")
        .remove("content_index");
    if let Some(part) = event.get_mut("part")
        && part.get("type").and_then(Value::as_str) == Some("reasoning_text")
    {
        part["type"] = Value::String("summary_text".to_owned());
    }
}

fn normalize_reasoning_item(item: &mut Value) {
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return;
    }
    if item
        .get("summary")
        .and_then(Value::as_array)
        .is_some_and(|summary| !summary.is_empty())
    {
        return;
    }
    let summary: Vec<_> = item
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("reasoning_text"))
        .filter_map(|part| {
            Some(serde_json::json!({
                "type": "summary_text",
                "text": part.get("text")?.as_str()?
            }))
        })
        .collect();
    if summary.is_empty() {
        return;
    }
    item["summary"] = Value::Array(summary);
    item["content"] = Value::Array(Vec::new());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_websocket_plan_prefers_direct_then_http_bridge() {
        assert_eq!(
            websocket_ingress_plan(&TransportConfig {
                http_sse: true,
                websocket: true,
            }),
            Some(ResponsesWebSocketPlan::DirectWebSocket)
        );
        assert_eq!(
            websocket_ingress_plan(&TransportConfig {
                http_sse: true,
                websocket: false,
            }),
            Some(ResponsesWebSocketPlan::BridgeToHttpSse)
        );
    }

    #[test]
    fn raw_reasoning_is_normalized_but_outcome_keeps_original_output() {
        let mut decoder = SseStreamDecoder::default();
        let events = decoder
            .push(b"data: {\"type\":\"response.reasoning_text.delta\",\"item_id\":\"r1\",\"content_index\":0,\"delta\":\"inspect\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"p1\",\"output\":[{\"type\":\"reasoning\",\"id\":\"r1\",\"summary\":[],\"content\":[{\"type\":\"reasoning_text\",\"text\":\"inspect\"}]}]}}\n\n")
            .unwrap();
        let delta: Value = serde_json::from_str(&events[0]).unwrap();
        let completed: Value = serde_json::from_str(&events[1]).unwrap();
        assert_eq!(delta["type"], "response.reasoning_summary_text.delta");
        assert_eq!(
            completed["response"]["output"][0]["summary"][0]["type"],
            "summary_text"
        );
        let outcome = decoder.outcome();
        assert_eq!(outcome.output[0]["content"][0]["type"], "reasoning_text");
        assert!(outcome.output[0]["summary"].as_array().unwrap().is_empty());
    }

    #[test]
    fn reasoning_source_table_rejects_more_than_256_unique_item_ids() {
        let mut decoder = SseStreamDecoder::new(64 * 1024);
        for index in 0..256 {
            let event = format!(
                "data: {}\n\n",
                serde_json::json!({
                    "type": "response.reasoning_text.delta",
                    "item_id": format!("r{index}"),
                    "content_index": 0,
                    "delta": "x"
                })
            );
            decoder.push(event.as_bytes()).unwrap();
        }

        let overflow = b"data: {\"type\":\"response.reasoning_text.delta\",\"item_id\":\"overflow\",\"content_index\":0,\"delta\":\"x\"}\n\n";
        assert_eq!(
            decoder.push(overflow).unwrap_err(),
            ProtocolError::StreamStateLimit
        );
    }

    #[test]
    fn reasoning_source_table_counts_item_id_bytes_against_the_stream_limit() {
        let mut decoder = SseStreamDecoder::new(512);
        for suffix in ['a', 'b'] {
            let event = format!(
                "data: {}\n\n",
                serde_json::json!({
                    "type": "response.reasoning_text.delta",
                    "item_id": format!("{}{suffix}", "r".repeat(199)),
                    "content_index": 0,
                    "delta": "x"
                })
            );
            assert!(event.len() < 512);
            decoder.push(event.as_bytes()).unwrap();
        }

        let overflow = format!(
            "data: {}\n\n",
            serde_json::json!({
                "type": "response.reasoning_text.delta",
                "item_id": format!("{}c", "r".repeat(199)),
                "content_index": 0,
                "delta": "x"
            })
        );
        assert_eq!(
            decoder.push(overflow.as_bytes()).unwrap_err(),
            ProtocolError::StreamStateLimit
        );
    }

    #[test]
    fn completed_reasoning_items_release_their_tracking_budget() {
        let mut decoder = SseStreamDecoder::new(64 * 1024);
        for index in 0..300 {
            let item_id = format!("r{index}");
            let delta = format!(
                "data: {}\n\n",
                serde_json::json!({
                    "type": "response.reasoning_text.delta",
                    "item_id": &item_id,
                    "content_index": 0,
                    "delta": "x"
                })
            );
            decoder.push(delta.as_bytes()).unwrap();
            let done = format!(
                "data: {}\n\n",
                serde_json::json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "reasoning",
                        "id": &item_id,
                        "summary": [],
                        "content": [{"type": "reasoning_text", "text": "x"}]
                    }
                })
            );
            decoder.push(done.as_bytes()).unwrap();
        }
    }

    #[test]
    fn tool_continuation_replays_committed_history() {
        let mut session = WsHttpBridgeSession::new("coder".to_owned(), 64 * 1024);
        let BridgeAction::Request(first) = session
            .prepare(r#"{"type":"response.create","model":"provider/coder","input":"pwd"}"#)
            .unwrap()
        else {
            panic!("expected request")
        };
        session
            .commit(
                first.input,
                vec![serde_json::json!({
                    "type": "function_call",
                    "call_id": "call_pwd",
                    "name": "exec_command",
                    "arguments": "{\"cmd\":\"pwd\"}"
                })],
            )
            .unwrap();
        let BridgeAction::Request(next) = session
            .prepare(r#"{"type":"response.create","model":"provider/coder","previous_response_id":"remote","input":[{"type":"function_call_output","call_id":"call_pwd","output":"/tmp"}]}"#)
            .unwrap()
        else {
            panic!("expected request")
        };
        let body: Value = serde_json::from_slice(&next.body).unwrap();
        assert!(body.get("previous_response_id").is_none());
        assert_eq!(body["input"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn warmup_events_and_defaults_are_protocol_owned() {
        let mut session = WsHttpBridgeSession::new("coder".to_owned(), 64 * 1024);
        let BridgeAction::Warmup { events } = session
            .prepare(
                r#"{"type":"response.create","model":"provider/coder","generate":false,"instructions":"be exact","tools":[{"type":"function","name":"lookup"}],"parallel_tool_calls":false}"#,
            )
            .unwrap()
        else {
            panic!("expected warmup")
        };
        assert_eq!(events.len(), 2);
        assert_eq!(
            serde_json::from_str::<Value>(&events[0]).unwrap()["type"],
            "response.created"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&events[1]).unwrap()["type"],
            "response.completed"
        );

        let BridgeAction::Request(request) = session
            .prepare(r#"{"type":"response.create","model":"provider/coder","input":"hi"}"#)
            .unwrap()
        else {
            panic!("expected request")
        };
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["instructions"], "be exact");
        assert_eq!(body["tools"][0]["name"], "lookup");
        assert_eq!(body["parallel_tool_calls"], false);
    }

    #[test]
    fn explicit_defaults_override_warmup_and_persist() {
        let mut session = WsHttpBridgeSession::new("coder".to_owned(), 64 * 1024);
        session
            .prepare(
                r#"{"type":"response.create","model":"provider/coder","generate":false,"instructions":"old"}"#,
            )
            .unwrap();
        let BridgeAction::Request(first) = session
            .prepare(
                r#"{"type":"response.create","model":"provider/coder","instructions":"new","input":"one"}"#,
            )
            .unwrap()
        else {
            panic!("expected request")
        };
        let first_body: Value = serde_json::from_slice(&first.body).unwrap();
        assert_eq!(first_body["instructions"], "new");

        let BridgeAction::Request(second) = session
            .prepare(r#"{"type":"response.create","model":"provider/coder","input":"two"}"#)
            .unwrap()
        else {
            panic!("expected request")
        };
        let second_body: Value = serde_json::from_slice(&second.body).unwrap();
        assert_eq!(second_body["instructions"], "new");
    }

    #[test]
    fn orphaned_tool_output_still_replays_history() {
        let mut session = WsHttpBridgeSession::new("coder".to_owned(), 64 * 1024);
        let BridgeAction::Request(first) = session
            .prepare(r#"{"type":"response.create","model":"provider/coder","input":"pwd"}"#)
            .unwrap()
        else {
            panic!("expected request")
        };
        session
            .commit(
                first.input,
                vec![serde_json::json!({
                    "type": "function_call",
                    "call_id": "call_pwd",
                    "name": "exec_command",
                    "arguments": "{\"cmd\":\"pwd\"}"
                })],
            )
            .unwrap();
        let BridgeAction::Request(next) = session
            .prepare(r#"{"type":"response.create","model":"provider/coder","input":[{"type":"function_call_output","call_id":"call_pwd","output":"/tmp"}]}"#)
            .unwrap()
        else {
            panic!("expected request")
        };
        assert_eq!(next.input.len(), 3);
    }

    #[test]
    fn repeated_history_prefix_is_not_duplicated() {
        let mut session = WsHttpBridgeSession::new("coder".to_owned(), 64 * 1024);
        let user = serde_json::json!({"type":"message","id":"m1","role":"user","content":"pwd"});
        let call = serde_json::json!({
            "type": "function_call",
            "call_id": "call_pwd",
            "name": "exec_command",
            "arguments": "{\"cmd\":\"pwd\"}"
        });
        session.commit(vec![user], vec![call.clone()]).unwrap();
        let message = serde_json::json!({
            "type": "response.create",
            "model": "provider/coder",
            "previous_response_id": "remote",
            "input": [
                call,
                {"type":"function_call_output","call_id":"call_pwd","output":"/tmp"}
            ]
        })
        .to_string();
        let BridgeAction::Request(next) = session.prepare(&message).unwrap() else {
            panic!("expected request")
        };
        assert_eq!(next.input.len(), 3);
    }
}
