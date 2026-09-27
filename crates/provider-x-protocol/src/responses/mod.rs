mod bridge;
mod error;
mod http;
mod inspect;
mod model_list;
mod paths;
mod rewrite;
mod websocket;

pub use bridge::{
    BridgeAction, BridgeRequest, ResponsesWebSocketPlan, ResponsesWsHttpAdapter, SseStreamDecoder,
    SseStreamOutcome, WsHttpBridgeSession, websocket_ingress_plan,
};
pub use error::ProtocolError;
pub use http::{http_error_body, inspect_http, rewrite_http_model};
pub use inspect::{InspectedRequest, StandardMetadata};
pub use model_list::{DiscoveredModel, model_list_url, parse_model_list};
pub use paths::{ResponsesPath, responses_url};
pub use websocket::{
    WebSocketMessageKind, classify_ws_text, inspect_ws_text, is_terminal_ws_event, rewrite_ws_text,
    websocket_error_event,
};
