use provider_x_core::RouteDecision;
use serde::Serialize;

/// Transport observed at the local ingress or selected upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedTransport {
    Http,
    WebSocket,
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

/// A request-routing observation containing only fields approved for M0 evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RequestObserved {
    pub transport: ObservedTransport,
    pub path: String,
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
    pub route: ObservedRoute,
    pub status: u16,
}

/// A deliberate local transport fallback signal containing no request content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FallbackObserved {
    pub transport: ObservedTransport,
    pub path: String,
    pub status: u16,
}

/// Redacted events emitted by the data plane for contract validation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EgressEvent {
    RequestObserved(RequestObserved),
    UpstreamObserved(UpstreamObserved),
    FallbackObserved(FallbackObserved),
}

/// Receives redacted contract events. Production uses a no-op observer by default.
pub trait EgressObserver: Send + Sync + 'static {
    fn record(&self, event: EgressEvent);
}

pub(crate) struct NoopObserver;

impl EgressObserver for NoopObserver {
    fn record(&self, _event: EgressEvent) {}
}

#[cfg(test)]
mod tests {
    use super::{EgressEvent, ObservedRoute, ObservedTransport, RequestObserved};

    #[test]
    fn serialized_contract_event_contains_no_request_content_or_credentials() {
        let event = EgressEvent::RequestObserved(RequestObserved {
            transport: ObservedTransport::WebSocket,
            path: "/v1/responses".to_owned(),
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
}
