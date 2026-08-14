use protocol_openai_responses::{
    ProtocolError, WebSocketMessageKind, classify_ws_text, http_error_body, inspect_http,
    inspect_ws_text, is_terminal_ws_event, responses_url, rewrite_http_model, rewrite_ws_text,
    websocket_error_event,
};
use serde_json::{Value, json};

#[test]
fn official_http_request_can_be_inspected_without_touching_bytes() {
    let body = br#"{ "model": "gpt-5.6", "input": "hello", "unknown": {"model":"nested"} }"#;
    let inspected = inspect_http("/v1/responses", body).unwrap();
    assert_eq!(inspected.model, "gpt-5.6");
    assert_eq!(
        body,
        br#"{ "model": "gpt-5.6", "input": "hello", "unknown": {"model":"nested"} }"#
    );
}

#[test]
fn compact_path_uses_the_same_top_level_model_contract() {
    let body = br#"{"model":"provider-a/coder","input":"compact me"}"#;
    let inspected = inspect_http("/v1/responses/compact", body).unwrap();
    assert_eq!(inspected.model, "provider-a/coder");
}

#[test]
fn third_party_http_rewrite_only_changes_top_level_model_semantics() {
    let body = br#"{
        "model": "provider-a/coder",
        "input": [{"role":"user","content":"hello"}],
        "nested": {"model":"must-not-change"},
        "client_metadata": {"x-openai-subagent":"review"},
        "future_field": [1, true, null]
    }"#;
    let before: Value = serde_json::from_slice(body).unwrap();
    let rewritten = rewrite_http_model(body, "coder").unwrap();
    let after: Value = serde_json::from_slice(&rewritten).unwrap();

    assert_eq!(after["model"], "coder");
    assert_eq!(after["nested"], before["nested"]);
    assert_eq!(after["client_metadata"], before["client_metadata"]);
    assert_eq!(after["future_field"], before["future_field"]);
    assert_eq!(after["input"], before["input"]);
}

#[test]
fn websocket_rewrite_preserves_standard_metadata_and_other_fields() {
    let message = json!({
        "type": "response.create",
        "model": "provider-a/coder",
        "previous_response_id": "resp_123",
        "client_metadata": {
            "x-openai-subagent": "review",
            "x-codex-turn-metadata": "opaque"
        }
    })
    .to_string();

    let inspected = inspect_ws_text(&message).unwrap();
    assert_eq!(inspected.model, "provider-a/coder");
    assert!(inspected.metadata.client_metadata.is_some());

    let rewritten: Value =
        serde_json::from_str(&rewrite_ws_text(&message, "coder").unwrap()).unwrap();
    assert_eq!(rewritten["model"], "coder");
    assert_eq!(rewritten["previous_response_id"], "resp_123");
    assert_eq!(rewritten["client_metadata"]["x-openai-subagent"], "review");
}

#[test]
fn websocket_rejects_non_create_messages_for_routing() {
    let error =
        inspect_ws_text(r#"{"type":"response.cancel","model":"provider-a/coder"}"#).unwrap_err();
    assert_eq!(error, ProtocolError::UnsupportedWebSocketMessage);
}

#[test]
fn websocket_message_classification_and_local_errors_are_protocol_owned() {
    assert_eq!(
        classify_ws_text(r#"{"type":"response.create","model":"x"}"#).unwrap(),
        WebSocketMessageKind::ResponseCreate
    );
    assert_eq!(
        classify_ws_text(r#"{"type":"response.cancel"}"#).unwrap(),
        WebSocketMessageKind::Other
    );
    assert!(classify_ws_text("not json").is_err());

    let ws_error: Value =
        serde_json::from_str(&websocket_error_event("route_changed", "cannot switch")).unwrap();
    assert_eq!(ws_error["type"], "error");
    assert_eq!(ws_error["error"]["code"], "route_changed");

    let http_error: Value = serde_json::from_slice(&http_error_body("bad request")).unwrap();
    assert_eq!(http_error["error"]["message"], "bad request");
}

#[test]
fn invalid_or_missing_model_fails_closed() {
    assert_eq!(
        inspect_http("/v1/responses", br#"{"input":"hello"}"#).unwrap_err(),
        ProtocolError::InvalidModel
    );
    assert_eq!(
        inspect_http("/v1/chat/completions", br#"{"model":"gpt-5.6"}"#).unwrap_err(),
        ProtocolError::UnsupportedPath("/v1/chat/completions".to_owned())
    );
}

#[test]
fn responses_url_uses_the_protocol_path_once() {
    assert_eq!(
        responses_url("https://provider.example/v1"),
        "https://provider.example/v1/responses"
    );
    assert_eq!(
        responses_url("https://provider.example/v1/"),
        "https://provider.example/v1/responses"
    );
}

#[test]
fn terminal_websocket_events_end_only_the_current_generation() {
    assert!(is_terminal_ws_event(
        r#"{"type":"response.completed","response":{"id":"r1"}}"#
    ));
    assert!(is_terminal_ws_event(
        r#"{"type":"response.incomplete","response":{"id":"r1"}}"#
    ));
    assert!(!is_terminal_ws_event(
        r#"{"type":"response.output_text.delta","delta":"hi"}"#
    ));
    assert!(!is_terminal_ws_event("not json"));
}
