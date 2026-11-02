//! Session-scoped SSE streams over HTTP: an [`HttpClient`] talks to an
//! [`AcpHttpServer`] hosting a v1 agent.
//!
//! The server only keeps session streams for sessions the agent names, so
//! these tests pin the flows that must keep working: a stream opened right
//! after `session/new`, a stream opened before `session/load`, and a request
//! for a session the agent does not know.
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

/// A v1 agent with one session, `created`, and one stored session,
/// `stored`, that `session/load` replays.
fn agent() -> impl ConnectTo<Client> {
    Agent.v1().on_receive_request(
        async |request: UntypedMessage, responder: Responder<Value, V1>, cx| {
            let session_id = request.params()["sessionId"].as_str().map(str::to_owned);
            match (request.method(), session_id.as_deref()) {
                ("initialize", _) => responder.respond(json!({ "protocolVersion": 1 })),
                ("session/new", _) => responder.respond(json!({ "sessionId": "created" })),
                ("session/prompt", Some("created")) | ("session/load", Some("stored")) => {
                    cx.send_notification(UntypedMessage::new(
                        "session/update",
                        json!({ "sessionId": session_id, "update": { "for": request.method() } }),
                    )?)?;
                    responder.respond(json!({}))
                }
                (method, _) => responder.respond_with_error(
                    agent_client_protocol::Error::invalid_params().data(method),
                ),
            }
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

    fn request(&self, id: u64, method: &str, params: Value) {
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.channel
            .tx
            .unbounded_send(TransportFrame::parse_json(&request.to_string()))
            .unwrap();
    }

    async fn recv(&mut self) -> Value {
        let frame = timeout(DEADLINE, self.channel.rx.next())
            .await
            .expect("agent should answer in time")
            .expect("transport closed");
        serde_json::from_str(&frame.to_json().unwrap()).unwrap()
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

#[tokio::test]
async fn session_streams_reach_created_loaded_and_unknown_sessions() {
    let (address, server) = serve().await;
    let mut peer = Peer::connect(HttpClient::new(format!("http://{address}")).unwrap());

    peer.request(1, "initialize", json!({ "protocolVersion": 1 }));
    assert_eq!(peer.recv().await["result"]["protocolVersion"], 1);

    // The client opens the session stream right after the `session/new`
    // response arrives.
    peer.request(2, "session/new", json!({ "cwd": "/", "mcpServers": [] }));
    assert_eq!(peer.recv().await["result"]["sessionId"], "created");
    peer.request(
        3,
        "session/prompt",
        json!({ "sessionId": "created", "prompt": [] }),
    );
    let update = peer.recv().await;
    assert_eq!(update["method"], "session/update");
    assert_eq!(update["params"]["update"]["for"], "session/prompt");
    assert_eq!(peer.recv().await["id"], 3);

    // The client opens the stream of a session it is about to load before
    // sending `session/load`, so the replay arrives on it.
    peer.request(
        4,
        "session/load",
        json!({ "sessionId": "stored", "cwd": "/", "mcpServers": [] }),
    );
    let update = peer.recv().await;
    assert_eq!(update["method"], "session/update");
    assert_eq!(update["params"]["update"]["for"], "session/load");
    assert_eq!(peer.recv().await["id"], 4);

    // A request for a session the agent does not know still gets its error.
    peer.request(
        5,
        "session/prompt",
        json!({ "sessionId": "made-up", "prompt": [] }),
    );
    let error = peer.recv().await;
    assert_eq!(error["id"], 5);
    assert_eq!(error["error"]["code"], -32602);

    peer.close().await;
    server.abort();
}
