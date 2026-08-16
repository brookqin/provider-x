use protocol_anthropic_messages::{
    AnthropicMessagesWsHttpAdapter, AnthropicSseDecoder, messages_url, model_list_url,
    prepare_http_request, prepare_http_request_with_thinking_mode,
};
use provider_x_core::AnthropicThinkingMode;
use provider_x_protocol::{WsHttpAction, WsHttpEventDecoder, WsHttpProtocolAdapter};
use serde_json::Value;

#[test]
fn responses_request_maps_to_anthropic_messages() {
    let request = prepare_http_request_with_thinking_mode(
        br#"{"model":"deepseek/deepseek-v4-pro","instructions":"Use tools precisely","reasoning":{"effort":"xhigh"},"max_output_tokens":12000,"parallel_tool_calls":false,"tools":[{"type":"namespace","name":"codex_app","tools":[{"type":"function","name":"exec_command","description":"run","parameters":{"type":"object"}}]}],"tool_choice":"auto","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"pwd"}]}]}"#,
        "deepseek-v4-pro",
        64 * 1024,
        AnthropicThinkingMode::Enabled,
    )
    .unwrap();
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["model"], "deepseek-v4-pro");
    assert_eq!(body["system"], "Use tools precisely");
    assert_eq!(body["messages"][0]["content"], "pwd");
    assert_eq!(body["tools"][0]["name"], "codex_app__exec_command");
    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    assert_eq!(body["tool_choice"]["type"], "auto");
    assert_eq!(body["tool_choice"]["disable_parallel_tool_use"], true);
    assert_eq!(body["max_tokens"], 12_000);
    assert_eq!(body["output_config"]["effort"], "max");
    assert_eq!(body["thinking"]["type"], "enabled");
}

#[test]
fn enabled_thinking_downgrades_forced_tool_choice_without_disabling_thinking() {
    let request = prepare_http_request_with_thinking_mode(
        br#"{"model":"deepseek/x","input":"call it","tools":[{"type":"function","name":"marker","parameters":{"type":"object"}}],"tool_choice":{"type":"function","name":"marker"}}"#,
        "x",
        4096,
        AnthropicThinkingMode::Enabled,
    )
    .unwrap();
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["tool_choice"]["type"], "auto");
    assert!(body["system"].as_str().unwrap().contains("marker"));
    assert_ne!(body["thinking"]["type"], "disabled");
}

#[test]
fn automatic_tool_choice_leaves_provider_thinking_default_unchanged() {
    let request = prepare_http_request(
        br#"{"model":"provider/x","input":"call it if needed","tools":[{"type":"function","name":"marker","parameters":{"type":"object"}}],"tool_choice":"auto"}"#,
        "x",
        4096,
    )
    .unwrap();
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["tool_choice"]["type"], "auto");
    assert!(body.get("thinking").is_none());
}

#[test]
fn parallel_tool_calls_false_creates_an_auto_tool_choice() {
    let request = prepare_http_request(
        br#"{"model":"provider/x","input":"call it if needed","tools":[{"type":"function","name":"marker","parameters":{"type":"object"}}],"parallel_tool_calls":false}"#,
        "x",
        4096,
    )
    .unwrap();
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["tool_choice"]["type"], "auto");
    assert_eq!(body["tool_choice"]["disable_parallel_tool_use"], true);
}

#[test]
fn adaptive_thinking_is_used_for_native_anthropic_reasoning() {
    let request = prepare_http_request(
        br#"{"model":"provider/x","input":"think","reasoning":{"effort":"high"}}"#,
        "x",
        4096,
    )
    .unwrap();
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["output_config"]["effort"], "high");
}

#[test]
fn bridge_request_debug_redacts_body_and_messages() {
    let request = prepare_http_request(
        br#"{"model":"provider/x","input":"private prompt marker"}"#,
        "x",
        4096,
    )
    .unwrap();
    let debug = format!("{request:?}");
    assert!(!debug.contains("private prompt marker"));
    assert!(debug.contains("body_bytes"));
    assert!(debug.contains("message_count"));
}

#[test]
fn anthropic_sse_maps_reasoning_text_tools_usage_and_terminal_event() {
    let mut decoder = AnthropicSseDecoder::new(64 * 1024);
    let stream = [
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"content\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"inspect\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sign\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"ed\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"checking\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_pwd\",\"name\":\"exec_command\",\"input\":{}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"cmd\\\":\\\"pwd\\\"}\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":8}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ];
    let mut events = Vec::new();
    for chunk in stream {
        events.extend(decoder.push(chunk.as_bytes()).unwrap());
    }
    events.extend(decoder.finish().unwrap());
    let events = events
        .iter()
        .map(|event| serde_json::from_str::<Value>(event).unwrap())
        .collect::<Vec<_>>();
    assert!(events.iter().any(|event| {
        event["type"] == "response.reasoning_summary_text.delta" && event["delta"] == "inspect"
    }));
    assert!(events.iter().any(|event| {
        event["type"] == "response.output_text.delta" && event["delta"] == "checking"
    }));
    let completed = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .unwrap();
    assert_eq!(completed["response"]["usage"]["input_tokens"], 10);
    assert_eq!(completed["response"]["usage"]["output_tokens"], 8);
    assert_eq!(completed["response"]["output"][2]["call_id"], "call_pwd");
    let tool_events = events
        .iter()
        .filter(|event| {
            matches!(
                event["type"].as_str(),
                Some(
                    "response.function_call_arguments.delta"
                        | "response.function_call_arguments.done"
                        | "response.output_item.added"
                        | "response.output_item.done"
                )
            ) && (event["item"]["type"] == "function_call"
                || event["type"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("response.function_call_arguments")))
        })
        .collect::<Vec<_>>();
    assert!(!tool_events.is_empty());
    for event in tool_events {
        assert_eq!(event["output_index"], 2, "{event}");
    }
    let outcome = decoder.outcome();
    assert!(outcome.completed);
    assert_eq!(
        outcome.assistant_message["_provider_x_anthropic_content"][0]["signature"],
        "signed"
    );
    assert_eq!(
        outcome.assistant_message["_provider_x_anthropic_content"][2]["input"]["cmd"],
        "pwd"
    );
}

#[test]
fn anthropic_urls_append_protocol_paths() {
    assert_eq!(
        messages_url("https://api.deepseek.com/anthropic/"),
        "https://api.deepseek.com/anthropic/v1/messages"
    );
    assert_eq!(
        model_list_url("https://api.deepseek.com/anthropic"),
        "https://api.deepseek.com/anthropic/v1/models"
    );
}

#[test]
fn websocket_bridge_preserves_signed_thinking_and_tool_history() {
    let mut adapter = AnthropicMessagesWsHttpAdapter::new_session("coder".to_owned(), 64 * 1024);
    let warmup = adapter
        .prepare_action(
            r#"{"type":"response.create","model":"provider/coder","generate":false,"instructions":"Use tools","tools":[{"type":"function","name":"marker","parameters":{"type":"object"}}]}"#,
        )
        .unwrap();
    assert!(matches!(warmup, WsHttpAction::Warmup { .. }));
    let first = adapter
        .prepare_action(r#"{"type":"response.create","model":"provider/coder","input":"call it"}"#)
        .unwrap();
    let WsHttpAction::Request { pending, .. } = first else {
        panic!("expected request")
    };
    let mut decoder = adapter.new_decoder(64 * 1024);
    let stream = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_tool\",\"usage\":{\"input_tokens\":4,\"output_tokens\":1}}}\n\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"inspect\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"signed\"}}\n\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"marker\",\"input\":{\"value\":1}}}\n\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":6}}\n\ndata: {\"type\":\"message_stop\"}\n\n";
    decoder.push(stream.as_bytes()).unwrap();
    let outcome = decoder.into_outcome();
    assert!(outcome.completed);
    adapter.commit_outcome(pending, outcome.commit).unwrap();

    let second = adapter
        .prepare_action(
            r#"{"type":"response.create","model":"provider/coder","previous_response_id":"msg_tool","input":[{"type":"function_call_output","call_id":"call_1","output":"ok"}]}"#,
        )
        .unwrap();
    let WsHttpAction::Request { body, .. } = second else {
        panic!("expected request")
    };
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["messages"][1]["content"][0]["type"], "thinking");
    assert_eq!(body["messages"][1]["content"][0]["signature"], "signed");
    assert_eq!(body["messages"][1]["content"][1]["type"], "tool_use");
    assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call_1");
    assert!(body.get("thinking").is_none());
}

#[test]
fn enabled_thinking_websocket_bridge_keeps_thinking_on_for_forced_tools() {
    let mut adapter = AnthropicMessagesWsHttpAdapter::new_session_with_thinking_mode(
        "coder".to_owned(),
        64 * 1024,
        AnthropicThinkingMode::Enabled,
    );
    adapter
        .prepare_action(
            r#"{"type":"response.create","model":"provider/coder","generate":false,"tools":[{"type":"function","name":"marker","parameters":{"type":"object"}}]}"#,
        )
        .unwrap();
    let first = adapter
        .prepare_action(
            r#"{"type":"response.create","model":"provider/coder","reasoning":{"effort":"high"},"tool_choice":{"type":"function","name":"marker"},"input":"call it"}"#,
        )
        .unwrap();
    let WsHttpAction::Request { body, pending } = first else {
        panic!("expected request")
    };
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["tool_choice"]["type"], "auto");
    assert!(body["system"].as_str().unwrap().contains("marker"));

    let mut decoder = adapter.new_decoder(64 * 1024);
    decoder
        .push(
            b"data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_tool\",\"usage\":{\"input_tokens\":4}}}\n\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"inspect\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"signed\"}}\n\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"marker\",\"input\":{}}}\n\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":2}}\n\ndata: {\"type\":\"message_stop\"}\n\n",
        )
        .unwrap();
    let outcome = decoder.into_outcome();
    adapter.commit_outcome(pending, outcome.commit).unwrap();

    let second = adapter
        .prepare_action(
            r#"{"type":"response.create","model":"provider/coder","previous_response_id":"msg_tool","tool_choice":"auto","input":[{"type":"function_call_output","call_id":"call_1","output":"ok"}]}"#,
        )
        .unwrap();
    let WsHttpAction::Request { body, .. } = second else {
        panic!("expected request")
    };
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["messages"][1]["content"][0]["signature"], "signed");
}

#[test]
fn stateless_http_rejects_previous_response_id() {
    assert!(
        prepare_http_request(
            br#"{"model":"deepseek/x","previous_response_id":"resp_1","input":"again"}"#,
            "x",
            4096,
        )
        .is_err()
    );
}
