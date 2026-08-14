use bytes::Bytes;
use provider_x_protocol::{
    BridgeFailure, WsHttpAction, WsHttpEventDecoder, WsHttpProtocolAdapter, WsHttpStreamOutcome,
};

struct DummyAdapter;

struct DummyDecoder {
    terminal: bool,
}

impl WsHttpProtocolAdapter for DummyAdapter {
    type Pending = usize;
    type Commit = usize;
    type Decoder = DummyDecoder;

    fn new_session(_upstream_model: String, _max_session_bytes: usize) -> Self {
        Self
    }

    fn upstream_url(http_endpoint: &str) -> String {
        format!("{http_endpoint}/generate")
    }

    fn prepare_action(
        &mut self,
        response_create: &str,
    ) -> Result<WsHttpAction<Self::Pending>, BridgeFailure> {
        Ok(WsHttpAction::Request {
            body: Bytes::copy_from_slice(response_create.as_bytes()),
            pending: response_create.len(),
        })
    }

    fn new_decoder(&self, _max_buffer_bytes: usize) -> Self::Decoder {
        DummyDecoder { terminal: false }
    }

    fn commit_outcome(
        &mut self,
        pending: Self::Pending,
        commit: Self::Commit,
    ) -> Result<(), BridgeFailure> {
        assert_eq!(pending, commit);
        Ok(())
    }
}

impl WsHttpEventDecoder for DummyDecoder {
    type Commit = usize;

    fn push(&mut self, data: &[u8]) -> Result<Vec<String>, BridgeFailure> {
        self.terminal = true;
        Ok(vec![String::from_utf8_lossy(data).into_owned()])
    }

    fn finish(&mut self) -> Result<Vec<String>, BridgeFailure> {
        Ok(Vec::new())
    }

    fn is_terminal(&self) -> bool {
        self.terminal
    }

    fn into_outcome(self) -> WsHttpStreamOutcome<Self::Commit> {
        WsHttpStreamOutcome {
            terminal: self.terminal,
            completed: self.terminal,
            commit: 7,
        }
    }
}

#[test]
fn adapter_contract_keeps_protocol_data_out_of_the_network_runner() {
    let mut adapter = DummyAdapter::new_session("model".to_owned(), 1024);
    let WsHttpAction::Request { body, pending } = adapter.prepare_action("request").unwrap() else {
        panic!("expected request action");
    };
    assert_eq!(body, Bytes::from_static(b"request"));
    let mut adapter = DummyAdapter;
    let mut decoder = adapter.new_decoder(1024);
    assert_eq!(decoder.push(b"event").unwrap(), ["event"]);
    assert!(decoder.is_terminal());
    let outcome = decoder.into_outcome();
    assert!(outcome.completed);
    adapter.commit_outcome(pending, outcome.commit).unwrap();
    assert_eq!(
        DummyAdapter::upstream_url("https://example.test"),
        "https://example.test/generate"
    );
}
