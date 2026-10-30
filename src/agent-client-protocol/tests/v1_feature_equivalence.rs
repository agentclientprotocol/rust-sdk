//! The `unstable_protocol_v2` feature only adds items: it never changes how a
//! v1 connection behaves.
//!
//! These tests are deliberately not feature-gated. The same scenarios and the
//! same expected wire traffic run with and without the feature (the workspace
//! suite runs `--all-features` and `-p agent-client-protocol
//! --no-default-features`), so any feature-dependent v1 behavior fails one of
//! the two runs.

use std::{io, time::Duration};

use agent_client_protocol::V1;
use agent_client_protocol::{
    Agent, Client, ConnectTo, ConnectionTo, Dispatch, Error, HandleDispatchFrom, Handled, Lines,
    Proxy, Role, UntypedMessage,
    schema::{ProtocolVersion, v1},
};
use futures::{SinkExt as _, StreamExt as _, channel::mpsc};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(5);

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

    fn send(&self, message: &Value) {
        self.incoming
            .unbounded_send(Ok(message.to_string()))
            .expect("component is still reading");
    }

    async fn recv(&mut self) -> Value {
        let line = tokio::time::timeout(TIMEOUT, self.outgoing.next())
            .await
            .expect("component should answer in time")
            .expect("component closed its output");
        serde_json::from_str(&line).expect("component writes JSON")
    }

    async fn finish(self) {
        drop(self.incoming);
        tokio::time::timeout(TIMEOUT, self.task)
            .await
            .expect("component should finish after input EOF")
            .expect("component task should not panic")
            .expect("component should finish cleanly");
    }
}

/// A stable v1 proxy's default forwarding: `proxy/initialize` and
/// `session/new` are forwarded untyped, so fields the SDK does not know
/// survive in both directions, and session notifications for the new session
/// are routed back to the client.
#[tokio::test(flavor = "current_thread")]
async fn v1_proxy_default_forwarding_is_feature_independent() {
    // The test plays the conductor.
    let mut conductor = WirePeer::connect(Proxy.v1());

    let mut initialize_params = serde_json::to_value(v1::InitializeProxyRequest::from(
        v1::InitializeRequest::new(ProtocolVersion::V1),
    ))
    .expect("serialize proxy/initialize");
    initialize_params["_unknownRequestField"] = json!("kept by untyped forwarding");
    conductor.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "proxy/initialize",
        "params": initialize_params,
    }));

    let forwarded = conductor.recv().await;
    assert_eq!(forwarded["method"], "proxy/successor");
    assert_eq!(forwarded["params"]["method"], "initialize");
    let forwarded_initialize = &forwarded["params"]["params"];
    assert_eq!(
        *forwarded_initialize, initialize_params,
        "the proxy forwards the initialize params unchanged"
    );

    let mut initialize_response =
        serde_json::to_value(v1::InitializeResponse::new(ProtocolVersion::V1))
            .expect("serialize initialize response");
    initialize_response["_unknownResponseField"] = json!(true);
    conductor.send(&json!({
        "jsonrpc": "2.0",
        "id": forwarded["id"],
        "result": initialize_response,
    }));
    let response = conductor.recv().await;
    assert_eq!(response["id"], 1);
    assert_eq!(
        response["result"], initialize_response,
        "the proxy returns the initialize response unchanged"
    );

    let new_session_params = json!({
        "cwd": "/v1-equivalence",
        "mcpServers": [],
        "_unknownRequestField": true,
    });
    conductor.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": new_session_params,
    }));
    let forwarded = conductor.recv().await;
    assert_eq!(forwarded["method"], "proxy/successor");
    assert_eq!(forwarded["params"]["method"], "session/new");
    assert_eq!(
        forwarded["params"]["params"], new_session_params,
        "the proxy forwards the session/new request unchanged"
    );

    let new_session_response = json!({
        "sessionId": "v1-session",
        "_unknownResponseField": true,
    });
    conductor.send(&json!({
        "jsonrpc": "2.0",
        "id": forwarded["id"],
        "result": new_session_response,
    }));
    let response = conductor.recv().await;
    assert_eq!(response["id"], 2);
    assert_eq!(
        response["result"], new_session_response,
        "the proxy returns the session/new response unchanged"
    );

    // The successor's update for the new session reaches the client.
    let update = json!({
        "sessionId": "v1-session",
        "update": {
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "hello" },
        },
    });
    conductor.send(&json!({
        "jsonrpc": "2.0",
        "method": "proxy/successor",
        "params": { "method": "session/update", "params": update },
    }));
    let routed = conductor.recv().await;
    assert_eq!(routed["method"], "session/update");
    assert_eq!(routed["params"], update);

    // A plain `initialize` from the client is rejected.
    conductor.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "initialize",
        "params": serde_json::to_value(v1::InitializeRequest::new(ProtocolVersion::V1))
            .expect("serialize initialize"),
    }));
    let response = conductor.recv().await;
    assert_eq!(response["id"], 3);
    assert_eq!(response["error"]["code"], -32600);

    conductor.finish().await;
}

/// Captures one `session/update` notification.
struct CaptureUpdate(Option<tokio::sync::oneshot::Sender<Value>>);

impl HandleDispatchFrom<Agent> for CaptureUpdate {
    fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        _connection: ConnectionTo<Agent>,
    ) -> impl Future<Output = Result<Handled<Dispatch>, Error>> + Send {
        std::future::ready(match message {
            Dispatch::Notification(notification) if notification.method() == "session/update" => {
                if let Some(tx) = self.0.take() {
                    tx.send(notification.params().clone())
                        .expect("test is waiting for the update");
                }
                Ok(Handled::Yes)
            }
            message => Ok(Handled::No {
                message,
                retry: false,
            }),
        })
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        "CaptureUpdate"
    }
}

/// The `Agent` role's default rule: a stable v1 client keeps an unhandled
/// session message from its agent for retry until a session handler
/// registered later accepts it, as the v1 session helpers rely on after
/// `session/new`.
#[tokio::test(flavor = "current_thread")]
async fn v1_client_retries_unhandled_session_messages() {
    let (captured_tx, captured_rx) = tokio::sync::oneshot::channel();
    let mut captured_tx = Some(captured_tx);
    let client = Client.v1().on_receive_request(
        async move |request: UntypedMessage,
                    responder: agent_client_protocol::Responder<Value, V1>,
                    cx: ConnectionTo<Agent>| {
            assert_eq!(request.method(), "_test/register_session_handler");
            cx.add_dynamic_handler(CaptureUpdate(captured_tx.take()))?
                .detach();
            responder.respond(json!({}))
        },
        agent_client_protocol::on_receive_request!(),
    );
    // The test plays the agent.
    let mut agent = WirePeer::connect(client);

    // No handler accepts this yet; the default rule must keep it for retry.
    let update = json!({
        "sessionId": "v1-session",
        "update": {
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "early" },
        },
    });
    agent.send(&json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": update,
    }));
    agent.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "_test/register_session_handler",
        "params": {},
    }));
    let response = agent.recv().await;
    assert_eq!(response["id"], 1);

    let delivered = tokio::time::timeout(TIMEOUT, captured_rx)
        .await
        .expect("the retried session message should reach the new handler")
        .expect("handler captured the message");
    assert_eq!(delivered, update);

    agent.finish().await;
}
