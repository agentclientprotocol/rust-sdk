//! A v1 client sees the same conductor with or without the
//! `unstable_protocol_v2` feature.
//!
//! This test is deliberately not feature-gated: the same scenario and the same
//! expected wire traffic run in the default and `--all-features` builds.

use std::{
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use agent_client_protocol::{
    Agent, Client, ConnectTo, ConnectionTo, DynConnectTo, Error, Lines, Proxy, Responder,
    UntypedMessage,
    schema::{ProtocolVersion, v1},
};
use agent_client_protocol_conductor::ConductorImpl;
use futures::{SinkExt as _, StreamExt as _, channel::mpsc};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(5);

/// The requests the agent component received, in order.
type Received = Arc<Mutex<Vec<(String, Value)>>>;

fn recording_v1_agent(received: Received) -> impl ConnectTo<Client> {
    Agent.builder().on_receive_request(
        async move |request: UntypedMessage,
                    responder: Responder<Value>,
                    _cx: ConnectionTo<Client>| {
            received
                .lock()
                .expect("received lock")
                .push((request.method().to_string(), request.params().clone()));
            match request.method() {
                "initialize" => {
                    let mut response =
                        serde_json::to_value(v1::InitializeResponse::new(ProtocolVersion::V1))
                            .map_err(Error::into_internal_error)?;
                    response["_unknownResponseField"] = json!(true);
                    responder.respond(response)
                }
                "session/new" => responder.respond(json!({
                    "sessionId": "v1-session",
                    "_unknownResponseField": true,
                })),
                method => responder.respond_with_error(Error::method_not_found().data(method)),
            }
        },
        agent_client_protocol::on_receive_request!(),
    )
}

fn typed_value<T: serde::Serialize + serde::de::DeserializeOwned>(value: &Value) -> Value {
    let typed: T = serde_json::from_value(value.clone()).expect("value parses as the v1 type");
    serde_json::to_value(typed).expect("v1 type serializes")
}

/// A v1 chain (one stable v1 proxy and a v1 agent) runs the stable v1
/// initialization and forwarding: v1's fallback-to-latest for a request below
/// v1, typed v1 requests and responses, and the v1 proxy's session routing.
#[tokio::test(flavor = "current_thread")]
async fn v1_chain_is_feature_independent() {
    let received = Received::default();
    let instantiated_version = Arc::new(Mutex::new(None));
    let conductor = ConductorImpl::new_agent("v1-equivalence", {
        let received = Arc::clone(&received);
        let instantiated_version = Arc::clone(&instantiated_version);
        move |request: v1::InitializeRequest| async move {
            *instantiated_version.lock().expect("version lock") = Some(request.protocol_version);
            let proxies = vec![DynConnectTo::new(Proxy.builder())];
            let agent = DynConnectTo::new(recording_v1_agent(received));
            Ok((request, proxies, agent))
        }
    });

    let (incoming, incoming_rx) = mpsc::unbounded::<io::Result<String>>();
    let (outgoing_tx, mut outgoing) = mpsc::unbounded::<String>();
    let task = tokio::spawn(conductor.connect_to(Lines::new(
        outgoing_tx.sink_map_err(io::Error::other),
        incoming_rx,
    )));
    let send = |message: Value| {
        incoming
            .unbounded_send(Ok(message.to_string()))
            .expect("conductor is still reading");
    };
    let mut recv = async || -> Value {
        let line = tokio::time::timeout(TIMEOUT, outgoing.next())
            .await
            .expect("conductor should answer in time")
            .expect("conductor closed its output");
        serde_json::from_str(&line).expect("conductor writes JSON")
    };

    // A request below v1 is not a v2 selection: v1 falls back to v1.
    let mut initialize = serde_json::to_value(v1::InitializeRequest::new(ProtocolVersion::V1))
        .expect("serialize initialize");
    initialize["protocolVersion"] = json!(0);
    initialize["_unknownRequestField"] = json!(true);
    send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": initialize,
    }));
    let response = recv().await;
    assert_eq!(response["id"], 1, "{response}");
    let mut expected_response =
        serde_json::to_value(v1::InitializeResponse::new(ProtocolVersion::V1))
            .expect("serialize initialize response");
    expected_response["_unknownResponseField"] = json!(true);
    assert_eq!(
        response["result"], expected_response,
        "the chain returns the agent's initialize response unchanged"
    );
    assert_eq!(
        *instantiated_version.lock().expect("version lock"),
        Some(ProtocolVersion::V1),
        "the instantiator sees the v1 selection"
    );

    send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": { "cwd": "/v1-equivalence", "mcpServers": [], "_unknownRequestField": true },
    }));
    let response = recv().await;
    assert_eq!(response["id"], 2, "{response}");
    assert_eq!(
        response["result"],
        json!({ "sessionId": "v1-session", "_unknownResponseField": true }),
        "the chain returns the agent's session/new response unchanged"
    );

    let received = received.lock().expect("received lock").clone();
    let mut expected_initialize = initialize.clone();
    expected_initialize["protocolVersion"] = json!(1);
    assert_eq!(
        received,
        vec![
            (
                "initialize".to_string(),
                typed_value::<v1::InitializeRequest>(&expected_initialize),
            ),
            (
                "session/new".to_string(),
                json!({
                    "cwd": "/v1-equivalence",
                    "mcpServers": [],
                    "_unknownRequestField": true,
                }),
            ),
        ],
        "the agent receives typed v1 requests"
    );

    drop(incoming);
    tokio::time::timeout(TIMEOUT, task)
        .await
        .expect("conductor should finish after input EOF")
        .expect("conductor task should not panic")
        .expect("conductor should finish cleanly");
}
