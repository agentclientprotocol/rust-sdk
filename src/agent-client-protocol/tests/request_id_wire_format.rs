//! Pins how JSON-RPC request IDs travel through v1 and v2 connections.
//!
//! Each test drives an agent over newline-delimited JSON and compares the
//! exact lines it writes, so any change to an ID's wire form (`1` becoming
//! `"1"`, a large integer losing precision, `null` being rewritten) fails
//! here. IDs are checked in both directions: IDs a peer chooses for its
//! requests are echoed in the agent's responses, and IDs the agent chooses
//! for its own requests are matched against the peer's responses.

use std::{io, time::Duration};

use agent_client_protocol::schema::{ProtocolVersion, v1};
use agent_client_protocol::{
    Agent, ConnectTo, Error, JsonRpcRequest, JsonRpcResponse, Lines, Role,
};
use futures::{SinkExt as _, StreamExt as _, channel::mpsc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(5);

/// IDs that every protocol version accepts, as written on the wire.
const IDS: &[&str] = &[
    "0",
    "1",
    "-1",
    // Above 2^53, so a round trip through `f64` would change it.
    "9007199254740993",
    "9223372036854775807",
    "-9223372036854775808",
    r#""1""#,
    r#""""#,
    r#""req-1""#,
    r#""ünïcødé \"quoted\"""#,
];

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_test/echo", response = EchoResponse)]
struct EchoRequest {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_test/callback", response = EchoResponse)]
struct CallbackRequest {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
struct EchoResponse {}

/// One end of a newline-delimited JSON-RPC connection to `component`.
struct WirePeer {
    incoming: mpsc::UnboundedSender<io::Result<String>>,
    outgoing: mpsc::UnboundedReceiver<String>,
    task: tokio::task::JoinHandle<Result<(), Error>>,
}

impl WirePeer {
    fn connect<R: Role>(component: impl ConnectTo<R> + 'static) -> Self {
        let (incoming, incoming_rx) = mpsc::unbounded::<io::Result<String>>();
        let (outgoing_tx, outgoing) = mpsc::unbounded::<String>();
        let lines = Lines::new(outgoing_tx.sink_map_err(io::Error::other), incoming_rx);
        let task = tokio::spawn(component.connect_to(lines));
        Self {
            incoming,
            outgoing,
            task,
        }
    }

    fn send(&self, line: String) {
        self.incoming
            .unbounded_send(Ok(line))
            .expect("agent is still reading");
    }

    async fn recv(&mut self) -> String {
        tokio::time::timeout(TIMEOUT, self.outgoing.next())
            .await
            .expect("agent should answer in time")
            .expect("agent closed its output")
    }

    /// Sends `method` with the raw `id` and returns the agent's reply line.
    async fn request(&mut self, id: &str, method: &str, params: &Value) -> String {
        self.send(format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
        ));
        self.recv().await
    }

    /// Asks the agent to send a request of its own, answers it by echoing the
    /// agent's ID text verbatim, and checks the agent matched the answer.
    async fn callback(&mut self, id: &str) {
        self.send(format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"_test/callback","params":{{}}}}"#
        ));
        let request = self.recv().await;
        let agent_id = raw_id(&request);
        assert!(
            agent_id.starts_with('"'),
            "the SDK assigns string IDs: {request}"
        );
        assert_eq!(
            request,
            format!(r#"{{"jsonrpc":"2.0","id":{agent_id},"method":"_test/echo","params":{{}}}}"#)
        );
        self.send(format!(
            r#"{{"jsonrpc":"2.0","id":{agent_id},"result":{{}}}}"#
        ));
        assert_eq!(
            self.recv().await,
            format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{}}}}"#)
        );
    }

    async fn finish(self) {
        drop(self.incoming);
        tokio::time::timeout(TIMEOUT, self.task)
            .await
            .expect("agent should finish after input EOF")
            .expect("agent task should not panic")
            .expect("agent should finish cleanly");
    }
}

/// The text of the top-level `"id"` member of a line the SDK wrote.
fn raw_id(line: &str) -> &str {
    let start = line.find(r#""id":"#).expect("message has an id") + r#""id":"#.len();
    let rest = &line[start..];
    let end = if let Some(string) = rest.strip_prefix('"') {
        // Agent IDs are UUIDs, so they contain no escaped quotes.
        string.find('"').expect("string id is closed") + 2
    } else {
        rest.find([',', '}'])
            .expect("id is followed by another member")
    };
    &rest[..end]
}

/// Request IDs a v1 peer chooses, including `null`, come back exactly as
/// sent, and IDs the agent chooses match the peer's responses.
#[tokio::test(flavor = "current_thread")]
async fn v1_request_ids_round_trip_byte_for_byte() {
    let mut peer = WirePeer::connect(
        Agent
            .v1()
            .on_receive_request(
                async |_request: EchoRequest, responder, _cx| responder.respond(EchoResponse {}),
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async |_request: CallbackRequest, responder, cx| {
                    cx.send_request(EchoRequest {})
                        .forward_response_to(responder)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async |request: v1::InitializeRequest, responder, _cx| {
                    responder.respond(v1::InitializeResponse::new(request.protocol_version))
                },
                agent_client_protocol::on_receive_request!(),
            ),
    );

    let initialize = serde_json::to_value(v1::InitializeRequest::new(ProtocolVersion::V1)).unwrap();
    let response = peer.request("0", "initialize", &initialize).await;
    assert!(
        response.starts_with(r#"{"jsonrpc":"2.0","id":0,"result":"#),
        "{response}"
    );

    for id in IDS.iter().copied().chain(["null"]) {
        assert_eq!(
            peer.request(id, "_test/echo", &json!({})).await,
            format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{}}}}"#),
        );
    }
    for id in IDS.iter().copied().chain(["null"]) {
        peer.callback(id).await;
    }

    assert_out_of_range_ids_are_rejected(&mut peer).await;
    peer.finish().await;
}

/// Integers outside the 64-bit signed range and fractional numbers are not
/// accepted as IDs. The message is answered as an invalid request with
/// `id: null`, because its ID cannot be represented.
async fn assert_out_of_range_ids_are_rejected(peer: &mut WirePeer) {
    for id in ["9223372036854775808", "-9223372036854775809", "1.5"] {
        let response = peer.request(id, "_test/echo", &json!({})).await;
        assert!(
            response.starts_with(r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"#),
            "{id}: {response}"
        );
    }
}

#[cfg(feature = "unstable_protocol_v2")]
mod v2_connection {
    use agent_client_protocol::Agent;
    use agent_client_protocol::schema::{ProtocolVersion, v2};
    use serde_json::json;

    use super::{
        CallbackRequest, EchoRequest, EchoResponse, IDS, WirePeer,
        assert_out_of_range_ids_are_rejected,
    };

    /// Request IDs a v2 peer chooses come back exactly as sent, IDs the
    /// agent chooses match the peer's responses, and a `null` request ID is
    /// rejected with `id: null`.
    #[tokio::test(flavor = "current_thread")]
    async fn v2_request_ids_round_trip_byte_for_byte() {
        let mut peer = WirePeer::connect(
            Agent
                .v2()
                .on_receive_request(
                    async |_request: EchoRequest, responder, _cx| {
                        responder.respond(EchoResponse {})
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async |_request: CallbackRequest, responder, cx| {
                        cx.send_request(EchoRequest {})
                            .forward_response_to(responder)
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async |request: v2::InitializeRequest, responder, _cx| {
                        responder.respond(v2::InitializeResponse::new(
                            request.protocol_version,
                            v2::Implementation::new("agent", "1.0.0"),
                        ))
                    },
                    agent_client_protocol::on_receive_request!(),
                ),
        );

        let initialize = serde_json::to_value(v2::InitializeRequest::new(
            ProtocolVersion::V2,
            v2::Implementation::new("client", "1.0.0"),
        ))
        .unwrap();
        let response = peer.request("0", "initialize", &initialize).await;
        assert!(
            response.starts_with(r#"{"jsonrpc":"2.0","id":0,"result":"#),
            "{response}"
        );

        for &id in IDS {
            assert_eq!(
                peer.request(id, "_test/echo", &json!({})).await,
                format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{}}}}"#),
            );
        }
        for &id in IDS {
            peer.callback(id).await;
        }

        assert_eq!(
            peer.request("null", "_test/echo", &json!({})).await,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid request","data":"ACP protocol v2 request ids must be a string or an integer, not null"}}"#,
        );

        assert_out_of_range_ids_are_rejected(&mut peer).await;
        peer.finish().await;
    }
}
