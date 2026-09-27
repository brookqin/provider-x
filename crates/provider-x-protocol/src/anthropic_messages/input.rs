use super::AnthropicProtocolError as Error;
use crate::output::ToolIdentity;
use serde_json::{Value, json};
use std::collections::BTreeMap;

type Tools = (Vec<Value>, BTreeMap<String, ToolIdentity>);

pub(super) fn messages(input: Option<&Value>) -> Result<(Vec<Value>, Vec<String>), Error> {
    let items = match input {
        Some(Value::String(text)) => {
            return Ok((vec![json!({"role":"user","content":text})], Vec::new()));
        }
        Some(Value::Array(items)) => items.clone(),
        Some(item) if item.is_object() => vec![item.clone()],
        _ => return Err(Error::InvalidRequest),
    };
    let mut messages = Vec::new();
    let mut system = Vec::new();
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("message") | None if item.get("role").is_some() => {
                let role = item["role"].as_str().ok_or(Error::InvalidRequest)?;
                let content = content(&item["content"])?;
                match role {
                    "system" | "developer" => {
                        system.push(content.as_str().ok_or(Error::InvalidRequest)?.to_owned());
                    }
                    "user" | "assistant" => messages.push(json!({"role":role,"content":content})),
                    _ => return Err(Error::InvalidRequest),
                }
            }
            Some("function_call") => {
                let id = item["call_id"].as_str().ok_or(Error::InvalidRequest)?;
                let name = item["name"].as_str().ok_or(Error::InvalidRequest)?;
                let name = item
                    .get("namespace")
                    .and_then(Value::as_str)
                    .map_or_else(|| name.to_owned(), |ns| flatten(ns, name));
                let input: Value = serde_json::from_str(item["arguments"].as_str().unwrap_or("{}"))
                    .map_err(|_| Error::InvalidRequest)?;
                messages.push(json!({"role":"assistant","content":[{"type":"tool_use","id":id,"name":name,"input":input}]}));
            }
            Some("function_call_output") => {
                let id = item["call_id"].as_str().ok_or(Error::InvalidRequest)?;
                let output = &item["output"];
                let text = output
                    .as_str()
                    .map_or_else(|| output.to_string(), str::to_owned);
                messages.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":text}]}));
            }
            Some(
                "reasoning" | "web_search" | "web_search_call" | "tool_search_call"
                | "tool_search_output",
            ) => {}
            None if item.is_string() => messages.push(json!({"role":"user","content":item})),
            _ => return Err(Error::UnsupportedInput("input item".to_owned())),
        }
    }
    if messages.is_empty() {
        return Err(Error::InvalidRequest);
    }
    Ok((messages, system))
}

fn content(value: &Value) -> Result<Value, Error> {
    if value.is_string() {
        return Ok(value.clone());
    }
    let parts = value.as_array().ok_or(Error::InvalidRequest)?;
    let mut text = String::new();
    for part in parts {
        match part["type"].as_str() {
            Some("input_text" | "output_text" | "text") => {
                text.push_str(part["text"].as_str().ok_or(Error::InvalidRequest)?);
            }
            _ => return Err(Error::UnsupportedInput("content part".to_owned())),
        }
    }
    Ok(Value::String(text))
}

pub(super) fn tools(value: Option<&Value>) -> Result<Tools, Error> {
    let mut converted = Vec::new();
    let mut names = BTreeMap::new();
    if let Some(value) = value {
        for tool in value.as_array().ok_or(Error::InvalidRequest)? {
            match tool["type"].as_str() {
                Some("function") => push_tool(tool, None, &mut converted, &mut names)?,
                Some("namespace") => {
                    let namespace = tool["name"].as_str().ok_or(Error::InvalidRequest)?;
                    for child in tool["tools"].as_array().ok_or(Error::InvalidRequest)? {
                        push_tool(child, Some(namespace), &mut converted, &mut names)?;
                    }
                }
                Some("web_search" | "web_search_preview" | "tool_search") => {}
                _ => return Err(Error::UnsupportedInput("tool type".to_owned())),
            }
        }
    }
    Ok((converted, names))
}

fn push_tool(
    tool: &Value,
    namespace: Option<&str>,
    output: &mut Vec<Value>,
    names: &mut BTreeMap<String, ToolIdentity>,
) -> Result<(), Error> {
    if tool["type"] != "function" {
        return Err(Error::InvalidRequest);
    }
    let name = tool["name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .ok_or(Error::InvalidRequest)?;
    let wire_name = namespace.map_or_else(|| name.to_owned(), |ns| flatten(ns, name));
    if names
        .insert(
            wire_name.clone(),
            ToolIdentity {
                name: name.to_owned(),
                namespace: namespace.map(str::to_owned),
            },
        )
        .is_some()
    {
        return Err(Error::UnsupportedInput("duplicate tool name".to_owned()));
    }
    let mut converted = json!({"name":wire_name,"input_schema":tool.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object"}))});
    if let Some(description) = tool.get("description") {
        converted["description"] = description.clone();
    }
    output.push(converted);
    Ok(())
}

pub(super) fn tool_choice(
    choice: &Value,
    parallel: Option<bool>,
    names: &BTreeMap<String, ToolIdentity>,
) -> Result<Option<Value>, Error> {
    let mut converted = if let Some(kind) = choice.as_str() {
        match kind {
            "auto" | "none" => json!({"type":kind}),
            "required" => json!({"type":"any"}),
            _ => return Err(Error::InvalidRequest),
        }
    } else {
        if matches!(
            choice["type"].as_str(),
            Some("web_search" | "web_search_preview" | "tool_search")
        ) {
            return Ok(None);
        }
        let name = choice
            .get("name")
            .or_else(|| choice.get("function").and_then(|f| f.get("name")))
            .and_then(Value::as_str)
            .ok_or(Error::InvalidRequest)?;
        let namespace = choice.get("namespace").and_then(Value::as_str);
        let wire = names
            .iter()
            .find(|(_, id)| id.name == name && id.namespace.as_deref() == namespace)
            .map(|(wire, _)| wire)
            .ok_or(Error::InvalidRequest)?;
        json!({"type":"tool","name":wire})
    };
    if parallel == Some(false) {
        converted["disable_parallel_tool_use"] = Value::Bool(true);
    }
    Ok(Some(converted))
}

fn flatten(namespace: &str, name: &str) -> String {
    let mut result = String::new();
    for byte in namespace.bytes().chain(*b"__").chain(name.bytes()) {
        if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
            result.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(result, "_x{byte:02x}_");
        }
    }
    result
}
