use bytes::Bytes;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BridgeFailure {
    InvalidRequest,
    InvalidStream,
    SessionHistoryLimit,
}

pub enum WsHttpAction<Pending> {
    Warmup { events: Vec<String> },
    Request { body: Bytes, pending: Pending },
}

pub struct WsHttpStreamOutcome<Commit> {
    pub terminal: bool,
    pub completed: bool,
    pub commit: Commit,
}

pub trait WsHttpEventDecoder {
    type Commit;

    /// Decodes arbitrary upstream HTTP body bytes into downstream WebSocket events.
    ///
    /// # Errors
    ///
    /// Returns a protocol-neutral failure when framing, content, or bounds are invalid.
    fn push(&mut self, data: &[u8]) -> Result<Vec<String>, BridgeFailure>;

    /// Flushes any final buffered stream data.
    ///
    /// # Errors
    ///
    /// Returns a protocol-neutral failure when the final stream data is invalid.
    fn finish(&mut self) -> Result<Vec<String>, BridgeFailure>;
    fn is_terminal(&self) -> bool;
    fn into_outcome(self) -> WsHttpStreamOutcome<Self::Commit>;
}

pub trait WsHttpProtocolAdapter: Sized {
    type Pending;
    type Commit;
    type Decoder: WsHttpEventDecoder<Commit = Self::Commit>;

    fn new_session(upstream_model: String, max_session_bytes: usize) -> Self;
    fn upstream_url(http_endpoint: &str) -> String;

    /// Converts one Responses WebSocket command into a local event or upstream HTTP request.
    ///
    /// # Errors
    ///
    /// Returns a protocol-neutral failure when the command is invalid or exceeds session bounds.
    fn prepare_action(
        &mut self,
        response_create: &str,
    ) -> Result<WsHttpAction<Self::Pending>, BridgeFailure>;
    fn new_decoder(&self, max_buffer_bytes: usize) -> Self::Decoder;

    /// Commits successfully completed upstream state for a later command on the same connection.
    ///
    /// # Errors
    ///
    /// Returns a protocol-neutral failure when the resulting session history exceeds its bounds.
    fn commit_outcome(
        &mut self,
        pending: Self::Pending,
        commit: Self::Commit,
    ) -> Result<(), BridgeFailure>;
}
