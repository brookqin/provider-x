use provider_x_core::RouteDecision;
use serde::Serialize;

/// Transport observed at the local ingress or selected upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedTransport {
    Http,
    WebSocket,
}

/// Upstream transport selected for a local WebSocket session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedWebSocketMode {
    DirectWebSocket,
    HttpBridge,
}

/// Bounded phase names used to locate a WebSocket failure without recording request content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedWebSocketStage {
    FirstMessage,
    Route,
    RequestBuild,
    Connect,
    Handshake,
    ResponseHeaders,
    Relay,
    ResponseStream,
}

/// Side of the proxy on which a WebSocket failure was observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedWebSocketDirection {
    Downstream,
    Upstream,
}

/// Sanitized WebSocket failure category. It deliberately excludes raw connector error text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedWebSocketReason {
    Timeout,
    InvalidEndpoint,
    Transport,
    Handshake,
    UnexpectedEof,
    Protocol,
    Rejected,
}

/// Redacted routing result. It never contains credentials or request content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObservedRoute {
    Official,
    ThirdParty { provider_id: String },
    UnavailableManagedModel,
}

impl From<&RouteDecision> for ObservedRoute {
    fn from(decision: &RouteDecision) -> Self {
        match decision {
            RouteDecision::BuiltInOfficial => Self::Official,
            RouteDecision::ThirdParty { provider_id, .. } => Self::ThirdParty {
                provider_id: provider_id.to_string(),
            },
            RouteDecision::UnavailableManagedModel => Self::UnavailableManagedModel,
        }
    }
}

/// A redacted request-routing observation safe for contract evidence and runtime diagnostics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RequestObserved {
    pub transport: ObservedTransport,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<u64>,
    pub sequence: u64,
    pub model: String,
    pub route: ObservedRoute,
    pub previous_response_id_present: bool,
    pub client_metadata_present: bool,
    pub codex_turn_metadata_header_present: bool,
}

/// An upstream handshake or HTTP response-header observation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpstreamObserved {
    pub transport: ObservedTransport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<u64>,
    pub route: ObservedRoute,
    pub status: u16,
}

/// Safe, structured context attached only to WebSocket failures.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WebSocketFailureObserved {
    pub session_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<ObservedWebSocketMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<ObservedRoute>,
    pub stage: ObservedWebSocketStage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direction: Option<ObservedWebSocketDirection>,
    pub reason: ObservedWebSocketReason,
}

/// A deliberate local transport fallback signal containing no request content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FallbackObserved {
    pub transport: ObservedTransport,
    pub path: String,
    pub status: u16,
}

/// A redacted request failure. Messages are supplied only by typed proxy errors and never include
/// headers, query strings, request bodies, response bodies, or ingress capabilities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ErrorObserved {
    pub transport: ObservedTransport,
    pub method: String,
    pub path: String,
    pub ingress_authorized: bool,
    pub status: Option<u16>,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub websocket: Option<WebSocketFailureObserved>,
}

/// Redacted events emitted by the data plane for diagnostics and contract validation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EgressEvent {
    RequestObserved(RequestObserved),
    UpstreamObserved(UpstreamObserved),
    FallbackObserved(FallbackObserved),
    ErrorObserved(ErrorObserved),
}

/// Receives redacted events. Egress state defaults to a no-op observer; the app installs logging.
pub trait EgressObserver: Send + Sync + 'static {
    fn record(&self, event: EgressEvent);
}

pub(crate) struct NoopObserver;

impl EgressObserver for NoopObserver {
    fn record(&self, _event: EgressEvent) {}
}

#[cfg(test)]
mod tests {
    use super::{
        EgressEvent, ErrorObserved, ObservedRoute, ObservedTransport, ObservedWebSocketDirection,
        ObservedWebSocketMode, ObservedWebSocketReason, ObservedWebSocketStage, RequestObserved,
        WebSocketFailureObserved,
    };

    #[test]
    fn serialized_contract_event_contains_no_request_content_or_credentials() {
        let event = EgressEvent::RequestObserved(RequestObserved {
            transport: ObservedTransport::WebSocket,
            path: "/v1/responses".to_owned(),
            session_id: Some(7),
            sequence: 2,
            model: "gpt-5.6-sol".to_owned(),
            route: ObservedRoute::Official,
            previous_response_id_present: true,
            client_metadata_present: true,
            codex_turn_metadata_header_present: true,
        });
        let serialized = serde_json::to_string(&event).unwrap();
        assert!(serialized.contains("gpt-5.6-sol"));
        assert!(serialized.contains("previous_response_id_present"));
        assert!(!serialized.contains("authorization"));
        assert!(!serialized.contains("chatgpt-account-id"));
        assert!(!serialized.contains("request_body"));
    }

    #[test]
    fn serialized_error_event_contains_only_redacted_diagnostics() {
        let event = EgressEvent::ErrorObserved(ErrorObserved {
            transport: ObservedTransport::Http,
            method: "POST".to_owned(),
            path: "/v1/responses".to_owned(),
            ingress_authorized: true,
            status: Some(502),
            code: "upstream_connect_failed".to_owned(),
            message: "failed to connect to upstream".to_owned(),
            websocket: None,
        });
        let serialized = serde_json::to_string(&event).unwrap();
        assert!(serialized.contains("upstream_connect_failed"));
        assert!(!serialized.contains("authorization"));
        assert!(!serialized.contains("request_body"));
        assert!(!serialized.contains("response_body"));
    }

    #[test]
    fn serialized_websocket_error_contains_structured_failure_context() {
        let event = EgressEvent::ErrorObserved(ErrorObserved {
            transport: ObservedTransport::WebSocket,
            method: "GET".to_owned(),
            path: "/v1/responses".to_owned(),
            ingress_authorized: true,
            status: None,
            code: "websocket_transport_failed".to_owned(),
            message: "WebSocket transport failed".to_owned(),
            websocket: Some(WebSocketFailureObserved {
                session_id: 19,
                mode: Some(ObservedWebSocketMode::HttpBridge),
                route: Some(ObservedRoute::ThirdParty {
                    provider_id: "provider-a".to_owned(),
                }),
                stage: ObservedWebSocketStage::ResponseStream,
                direction: Some(ObservedWebSocketDirection::Upstream),
                reason: ObservedWebSocketReason::UnexpectedEof,
            }),
        });
        let serialized = serde_json::to_string(&event).unwrap();
        assert!(serialized.contains("\"session_id\":19"));
        assert!(serialized.contains("\"mode\":\"http_bridge\""));
        assert!(serialized.contains("\"provider_id\":\"provider-a\""));
        assert!(serialized.contains("\"stage\":\"response_stream\""));
        assert!(serialized.contains("\"direction\":\"upstream\""));
        assert!(serialized.contains("\"reason\":\"unexpected_eof\""));
        assert!(!serialized.contains("authorization"));
        assert!(!serialized.contains("endpoint"));
    }
}
