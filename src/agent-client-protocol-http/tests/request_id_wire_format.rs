//! Pins how JSON-RPC request IDs travel through the HTTP and WebSocket
//! transports.
//!
//! An [`HttpClient`] talks to an [`AcpHttpServer`] hosting a v1 agent. Every
//! request ID form JSON-RPC allows is sent in both directions, and each
//! message is compared as serialized JSON text, so an ID that changes form
//! (`1` becoming `"1"`, a large integer losing precision) or is routed to the
//! wrong request fails here.
#![cfg(all(feature = "client", feature = "server"))]

use std::time::Duration;

use agent_client_protocol::{
    Agent, Channel, Client, ConnectTo, ConnectionDriver, Responder, TransportFrame, UntypedMessage,
    V1,
};
use agent_client_protocol_http::{AcpHttpServer, HttpClient};
use futures::StreamExt as _;
use serde_json::{Value, json};
use tokio::{net::TcpListener, time::timeout};

const DEADLINE: Duration = Duration::from_secs(10);

/// Request IDs as written on the wire, including v1's `null`.
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
    "null",
];

/// A v1 agent that answers `_test/echo` with `{}` and, for `_test/callback`,
/// sends `_test/echo` to the client and forwards its answer.
fn agent() -> impl ConnectTo<Client> {
    Agent.builder().on_receive_request(
        async |request: UntypedMessage, responder: Responder<Value, V1>, cx| match request.method()
        {
            "initialize" => responder.respond(json!({ "protocolVersion": 1 })),
            "_test/echo" => responder.respond(json!({})),
            "_test/callback" => cx
                .send_request(UntypedMessage::new("_test/echo", json!({}))?)
                .forward_response_to(responder),
            method => Err(agent_client_protocol::Error::method_not_found().data(method)),
        },
        agent_client_protocol::on_receive_request!(),
    )
}

async fn serve() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = AcpHttpServer::new(agent).into_router();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (address.to_string(), server)
}

struct Peer {
    channel: Channel,
    driver: tokio::task::JoinHandle<agent_client_protocol::Result<()>>,
}

impl Peer {
    fn connect(client: HttpClient) -> Self {
        let (channel, driver): (Channel, Option<ConnectionDriver>) =
            ConnectTo::<Client>::into_channel_and_future(client);
        Self {
            channel,
            driver: tokio::spawn(driver.expect("HttpClient owns its transport driver")),
        }
    }

    fn send(&self, line: String) {
        self.channel
            .tx
            .unbounded_send(TransportFrame::parse_json(&line))
            .unwrap();
    }

    async fn recv(&mut self) -> String {
        timeout(DEADLINE, self.channel.rx.next())
            .await
            .expect("agent should answer in time")
            .expect("transport closed")
            .to_json()
            .unwrap()
    }

    async fn request(&mut self, id: &str, method: &str) -> String {
        let params = if method == "initialize" {
            r#"{"protocolVersion":1}"#
        } else {
            "{}"
        };
        self.send(format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
        ));
        self.recv().await
    }

    async fn close(self) {
        drop(self.channel);
        timeout(DEADLINE, self.driver)
            .await
            .expect("transport should finish")
            .unwrap()
            .unwrap();
    }
}

/// The text of the top-level `"id"` member of a serialized message.
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

async fn round_trip(client: HttpClient) {
    let mut peer = Peer::connect(client);
    assert_eq!(
        peer.request(r#""init""#, "initialize").await,
        r#"{"jsonrpc":"2.0","id":"init","result":{"protocolVersion":1}}"#
    );

    for &id in IDS {
        assert_eq!(
            peer.request(id, "_test/echo").await,
            format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{}}}}"#),
        );
    }

    for &id in IDS {
        peer.send(format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"_test/callback","params":{{}}}}"#
        ));
        let request = peer.recv().await;
        let agent_id = raw_id(&request).to_owned();
        assert_eq!(
            request,
            format!(r#"{{"jsonrpc":"2.0","id":{agent_id},"method":"_test/echo","params":{{}}}}"#)
        );
        peer.send(format!(
            r#"{{"jsonrpc":"2.0","id":{agent_id},"result":{{}}}}"#
        ));
        assert_eq!(
            peer.recv().await,
            format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{}}}}"#)
        );
    }

    peer.close().await;
}

#[tokio::test]
async fn http_request_ids_round_trip() {
    let (address, server) = serve().await;
    round_trip(HttpClient::new(format!("http://{address}")).unwrap()).await;
    server.abort();
}

#[tokio::test]
async fn websocket_request_ids_round_trip() {
    let (address, server) = serve().await;
    round_trip(HttpClient::new(format!("ws://{address}")).unwrap()).await;
    server.abort();
}

/// The initialize request's ID is echoed in the POST response, whatever its
/// form.
#[tokio::test]
async fn http_initialize_ids_round_trip() {
    let (address, server) = serve().await;
    for &id in IDS {
        let mut peer = Peer::connect(HttpClient::new(format!("http://{address}")).unwrap());
        assert_eq!(
            peer.request(id, "initialize").await,
            format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":1}}}}"#),
        );
        peer.close().await;
    }
    server.abort();
}
