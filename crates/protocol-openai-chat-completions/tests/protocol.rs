use protocol_openai_chat_completions::{
    ChatBridgeAction, ChatProtocolError, ChatSseDecoder, ResponsesChatBridgeSession,
    prepare_http_request,
};
use serde_json::Value;

#[test]
fn responses_request_maps_instructions_tools_reasoning_and_messages() {
    let request = prepare_http_request(
        br#"{"model":"deepseek/deepseek-v4-pro","instructions":"Use tools precisely","reasoning":{"effort":"xhigh"},"tools":[{"type":"function","name":"exec_command","description":"run","parameters":{"type":"object"}}],"tool_choice":"auto","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"pwd"}]}]}"#,
        "deepseek-v4-pro",
        64 * 1024,
    )
    .unwrap();
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["model"], "deepseek-v4-pro");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1]["content"], "pwd");
    assert_eq!(body["tools"][0]["function"]["name"], "exec_command");
    assert_eq!(body["reasoning_effort"], "max");
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["stream_options"]["include_usage"], true);
}

#[test]
fn namespace_functions_are_flattened_and_restored_in_responses_events() {
    let request = prepare_http_request(
        br#"{"model":"deepseek/x","input":"open it","tools":[{"type":"namespace","name":"codex_app","description":"desktop tools","tools":[{"type":"function","name":"open_in_codex","description":"open","parameters":{"type":"object"},"defer_loading":true}]}],"tool_choice":{"type":"function","name":"open_in_codex","namespace":"codex_app"}}"#,
        "x",
        64 * 1024,
    )
    .unwrap();
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(
        body["tools"][0]["function"]["name"],
        "codex_app__open_in_codex"
    );
    assert_eq!(
        body["tool_choice"]["function"]["name"],
        "codex_app__open_in_codex"
    );

    let mut decoder = request.decoder(64 * 1024);
    let events = decoder
        .push(
            b"data: {\"id\":\"chat-ns\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_open\",\"type\":\"function\",\"function\":{\"name\":\"codex_app__open_in_codex\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
        )
        .unwrap();
    let completed = events
        .iter()
        .map(|event| serde_json::from_str::<Value>(event).unwrap())
        .find(|event| event["type"] == "response.completed")
        .unwrap();
    let call = &completed["response"]["output"][0];
    assert_eq!(call["name"], "open_in_codex");
    assert_eq!(call["namespace"], "codex_app");
}

#[test]
fn warmup_defaults_and_tool_history_are_preserved() {
    let mut session = ResponsesChatBridgeSession::new("deepseek-v4-pro".to_owned(), 64 * 1024);
    let warmup = session.prepare(
        r#"{"type":"response.create","model":"deepseek/x","generate":false,"instructions":"exact","tools":[{"type":"function","name":"exec_command","parameters":{"type":"object"}}]}"#,
    ).unwrap();
    let ChatBridgeAction::Warmup { events } = warmup else {
        panic!("expected warmup")
    };
    assert_eq!(events.len(), 2);
    let first = session
        .prepare(r#"{"type":"response.create","model":"deepseek/x","input":"pwd"}"#)
        .unwrap();
    let ChatBridgeAction::Request(first) = first else {
        panic!("expected request")
    };
    let body: Value = serde_json::from_slice(&first.body).unwrap();
    assert_eq!(body["messages"][0]["content"], "exact");
    session.commit(first.messages, serde_json::json!({
        "role":"assistant", "content":null, "reasoning_content":"inspect",
        "tool_calls":[{"id":"call_pwd","type":"function","function":{"name":"exec_command","arguments":"{\"cmd\":\"pwd\"}"}}]
    })).unwrap();
    let second = session.prepare(
        r#"{"type":"response.create","model":"deepseek/x","previous_response_id":"remote","input":[{"type":"function_call_output","call_id":"call_pwd","output":"/tmp"}]}"#,
    ).unwrap();
    let ChatBridgeAction::Request(second) = second else {
        panic!("expected request")
    };
    let body: Value = serde_json::from_slice(&second.body).unwrap();
    assert_eq!(body["messages"][2]["role"], "assistant");
    assert_eq!(body["messages"][2]["reasoning_content"], "inspect");
    assert_eq!(body["messages"][3]["role"], "tool");
    assert_eq!(body["messages"][3]["tool_call_id"], "call_pwd");
}

#[test]
fn chat_sse_maps_reasoning_text_tools_usage_and_terminal_event() {
    let mut decoder = ChatSseDecoder::new(64 * 1024);
    let chunks = [
        "data: {\"id\":\"chat-1\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"inspect \"},\"finish_reason\":null}],\"usage\":null}\n\n",
        "data: {\"id\":\"chat-1\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"workspace\",\"content\":\"checking\",\"tool_calls\":[{\"index\":0,\"id\":\"call_pwd\",\"type\":\"function\",\"function\":{\"name\":\"exec_command\",\"arguments\":\"{\\\"cmd\\\":\"}}]},\"finish_reason\":null}],\"usage\":null}\n\n",
        "data: {\"id\":\"chat-1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"pwd\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":null}\n\n",
        "data: {\"id\":\"chat-1\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\ndata: [DONE]\n\n",
    ];
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(decoder.push(chunk.as_bytes()).unwrap());
    }
    events.extend(decoder.finish().unwrap());
    let events = events
        .iter()
        .map(|event| serde_json::from_str::<Value>(event).unwrap())
        .collect::<Vec<_>>();
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "response.reasoning_summary_text.delta")
    );
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "response.output_text.delta")
    );
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "response.function_call_arguments.delta")
    );
    let completed = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .unwrap();
    assert_eq!(completed["response"]["usage"]["total_tokens"], 15);
    assert_eq!(completed["response"]["output"][2]["call_id"], "call_pwd");
    let outcome = decoder.outcome();
    assert!(outcome.completed);
    assert_eq!(
        outcome.assistant_message["reasoning_content"],
        "inspect workspace"
    );
    assert_eq!(
        outcome.assistant_message["tool_calls"][0]["function"]["arguments"],
        "{\"cmd\":\"pwd\"}"
    );
}

#[test]
fn chat_sse_rejects_aggregate_text_larger_than_the_stream_limit() {
    let mut decoder = ChatSseDecoder::new(512);
    let content = "x".repeat(200);
    let chunk = format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "chat-large",
            "choices": [{
                "index": 0,
                "delta": {"content": content},
                "finish_reason": null
            }]
        })
    );
    assert!(chunk.len() < 512);

    decoder.push(chunk.as_bytes()).unwrap();
    decoder.push(chunk.as_bytes()).unwrap();
    assert_eq!(
        decoder.push(chunk.as_bytes()).unwrap_err(),
        ChatProtocolError::StreamStateLimit
    );
}

#[test]
fn chat_sse_rejects_more_than_256_tool_aggregation_states() {
    let mut decoder = ChatSseDecoder::new(64 * 1024);
    for index in 0..256 {
        let chunk = format!(
            "data: {}\n\n",
            serde_json::json!({
                "id": "chat-tools",
                "choices": [{
                    "index": 0,
                    "delta": {"tool_calls": [{"index": index}]},
                    "finish_reason": null
                }]
            })
        );
        decoder.push(chunk.as_bytes()).unwrap();
    }

    let overflow = format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "chat-tools",
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{"index": 256}]},
                "finish_reason": null
            }]
        })
    );
    assert_eq!(
        decoder.push(overflow.as_bytes()).unwrap_err(),
        ChatProtocolError::StreamStateLimit
    );
}

#[test]
fn chat_sse_rejects_repeated_finish_events_before_they_can_amplify_output() {
    let mut decoder = ChatSseDecoder::new(1024);
    let finished = b"data: {\"id\":\"chat-finished\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n";
    decoder.push(finished).unwrap();
    assert_eq!(
        decoder.push(finished).unwrap_err(),
        ChatProtocolError::InvalidStream
    );
}

#[test]
fn managed_search_tools_and_history_are_omitted_but_functions_remain() {
    let request = prepare_http_request(
        br#"{"model":"x","input":[{"type":"web_search_call","id":"search_1","status":"completed"},{"type":"message","role":"assistant","content":"Earlier search summary"},{"type":"message","role":"user","content":"continue"}],"tools":[{"type":"web_search"},{"type":"tool_search"},{"type":"function","name":"exec_command","parameters":{"type":"object"}}],"tool_choice":"auto","parallel_tool_calls":true}"#,
        "x",
        4096,
    )
    .unwrap();
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["tools"].as_array().unwrap().len(), 1);
    assert_eq!(body["tools"][0]["function"]["name"], "exec_command");
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    assert_eq!(body["messages"][0]["content"], "Earlier search summary");
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["parallel_tool_calls"], true);
}

#[test]
fn unsupported_non_function_tool_is_still_rejected() {
    let error = prepare_http_request(
        br#"{"model":"x","input":"hi","tools":[{"type":"computer_use_preview"}]}"#,
        "x",
        4096,
    )
    .unwrap_err();
    assert!(error.to_string().contains("computer_use_preview"));
}

#[test]
fn a_search_only_request_drops_tools_and_tool_choice_together() {
    let request = prepare_http_request(
        br#"{"model":"x","input":"hi","tools":[{"type":"web_search_preview"}],"tool_choice":{"type":"web_search_preview"},"parallel_tool_calls":true}"#,
        "x",
        4096,
    )
    .unwrap();
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
    assert!(body.get("parallel_tool_calls").is_none());
}

#[test]
fn stateless_http_rejects_previous_response_id() {
    let error = prepare_http_request(
        br#"{"model":"x","previous_response_id":"resp_1","input":"continue"}"#,
        "x",
        4096,
    )
    .unwrap_err();
    assert!(error.to_string().contains("stateful WebSocket bridge"));
}
