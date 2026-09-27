use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
const MAX_STREAM_TOOL_CALLS: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    #[error("invalid upstream stream")]
    InvalidStream,
    #[error("stream aggregation exceeds configured limit")]
    StreamStateLimit,
}

#[derive(Clone, Copy)]
pub enum StreamEvent<'a> {
    Start(&'a str),
    Usage {
        input: u64,
        output: u64,
    },
    Text(&'a str),
    Reasoning(&'a str),
    Tool {
        index: u64,
        id: Option<&'a str>,
        name: Option<&'a str>,
        arguments: Option<&'a str>,
    },
    Stop(&'a str),
    Done,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolIdentity {
    pub name: String,
    pub namespace: Option<String>,
}

#[derive(Default)]
struct ToolCallState {
    id: String,
    name: String,
    arguments: String,
    added: bool,
}

#[allow(clippy::struct_excessive_bools)] // Incremental SSE output has independent item lifecycles.
pub struct OutputEmitter {
    pending: VecDeque<String>,
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

pub struct OutputOutcome {
    pub terminal: bool,
    pub completed: bool,
    pub assistant_message: Value,
}

impl OutputEmitter {
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
            pending: VecDeque::new(),
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

    pub fn drain(&mut self) -> Vec<String> {
        self.pending.drain(..).collect()
    }

    pub fn finish(&mut self) -> Vec<String> {
        if !self.terminal && self.finish_reason.is_some() {
            self.finalize();
        }
        self.drain()
    }

    #[must_use]
    pub fn outcome(self) -> OutputOutcome {
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
        OutputOutcome {
            terminal: self.terminal,
            completed: self.completed,
            assistant_message: assistant,
        }
    }

    /// Accepts one decoded semantic event.
    /// # Errors
    /// Rejects invalid event order or aggregate state above the limit.
    pub fn apply(&mut self, event: StreamEvent<'_>) -> Result<(), OutputError> {
        if self.terminal {
            return Err(OutputError::InvalidStream);
        }
        match event {
            StreamEvent::Start(id) => {
                if self.response_id.is_none() {
                    self.reserve_aggregate_bytes(id.len())?;
                    self.response_id = Some(id.to_owned());
                    self.emit(json!({"type":"response.created","response":{"id":id}}));
                }
            }
            StreamEvent::Usage { input, output } => {
                let usage = json!({"input_tokens":input,"output_tokens":output,"total_tokens":input.saturating_add(output)});
                let size = usage.to_string().len();
                self.resize_aggregate_bytes(self.usage_bytes, size)?;
                self.usage = Some(usage);
                self.usage_bytes = size;
            }
            StreamEvent::Text(text) if !text.is_empty() => {
                self.start_text();
                self.reserve_aggregate_bytes(text.len())?;
                self.text.push_str(text);
                self.emit(json!({"type":"response.output_text.delta","item_id":self.message_id,"output_index":self.message_output_index(),"content_index":0,"delta":text}));
            }
            StreamEvent::Reasoning(text) if !text.is_empty() => {
                self.start_reasoning();
                self.reserve_aggregate_bytes(text.len())?;
                self.reasoning.push_str(text);
                self.emit(json!({"type":"response.reasoning_summary_text.delta","item_id":self.reasoning_id,"output_index":0,"summary_index":0,"delta":text}));
            }
            StreamEvent::Tool {
                index,
                id,
                name,
                arguments,
            } => {
                self.apply_tool_delta(
                    &json!({"index":index,"id":id,"function":{"name":name,"arguments":arguments}}),
                )?;
            }
            StreamEvent::Stop(reason) => {
                if self.finish_reason.is_some() {
                    return Err(OutputError::InvalidStream);
                }
                self.reserve_aggregate_bytes(reason.len())?;
                self.finish_reason = Some(reason.to_owned());
                self.finish_items();
            }
            StreamEvent::Done => {
                if self.finish_reason.is_none() {
                    return Err(OutputError::InvalidStream);
                }
                self.finalize();
            }
            StreamEvent::Text(_) | StreamEvent::Reasoning(_) => {}
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
    fn apply_tool_delta(&mut self, call: &Value) -> Result<(), OutputError> {
        let index = call
            .get("index")
            .and_then(Value::as_u64)
            .ok_or(OutputError::InvalidStream)?;
        let new_tool = !self.tools.contains_key(&index);
        if new_tool && self.tools.len() >= MAX_STREAM_TOOL_CALLS {
            return Err(OutputError::StreamStateLimit);
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
            .ok_or(OutputError::StreamStateLimit)?;
        if next_aggregate_bytes > self.max_aggregate_bytes {
            return Err(OutputError::StreamStateLimit);
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

    fn reserve_aggregate_bytes(&mut self, additional: usize) -> Result<(), OutputError> {
        let next = self
            .aggregate_bytes
            .checked_add(additional)
            .ok_or(OutputError::StreamStateLimit)?;
        if next > self.max_aggregate_bytes {
            return Err(OutputError::StreamStateLimit);
        }
        self.aggregate_bytes = next;
        Ok(())
    }

    fn resize_aggregate_bytes(&mut self, previous: usize, next: usize) -> Result<(), OutputError> {
        let resized = self
            .aggregate_bytes
            .saturating_sub(previous)
            .checked_add(next)
            .ok_or(OutputError::StreamStateLimit)?;
        if resized > self.max_aggregate_bytes {
            return Err(OutputError::StreamStateLimit);
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

fn response_usage(usage: Option<&Value>) -> Value {
    let input = usage
        .and_then(|usage| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .and_then(|usage| usage.get("output_tokens"))
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

fn random_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}
