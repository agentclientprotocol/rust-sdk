//! A v1 client sees the same conductor with or without the
//! `unstable_protocol_v2` feature.
//!
//! These tests are deliberately not feature-gated: the same scenarios and the
//! same expected wire traffic run in the default and `--all-features` builds.

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
use agent_client_protocol_conductor::{ConductorImpl, SelectedInitializeRequest};
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

fn initialize_params(protocol_version: u16) -> Value {
    let mut initialize = serde_json::to_value(v1::InitializeRequest::new(ProtocolVersion::V1))
        .expect("serialize initialize");
    initialize["protocolVersion"] = json!(protocol_version);
    initialize["_unknownRequestField"] = json!(true);
    initialize
}

/// What a v1 chain (one stable v1 proxy and a v1 agent) saw.
struct ChainRun {
    /// The conductor's answers to the client.
    responses: Vec<Value>,
    /// The requests the agent received.
    received: Vec<(String, Value)>,
    /// The version the instantiator was asked for.
    instantiated_version: Option<ProtocolVersion>,
}

/// Initialize a v1 chain with `initialize`, then create a session.
async fn run_v1_chain(initialize: &Value) -> ChainRun {
    let received = Received::default();
    let instantiated_version = Arc::new(Mutex::new(None));
    let conductor = ConductorImpl::new_agent("v1-equivalence", {
        let received = Arc::clone(&received);
        let instantiated_version = Arc::clone(&instantiated_version);
        move |request: SelectedInitializeRequest| async move {
            *instantiated_version.lock().expect("version lock") = Some(request.protocol_version());
            let request = request.into_v1()?;
            let proxies = vec![DynConnectTo::new(Proxy.builder())];
            let agent = DynConnectTo::new(recording_v1_agent(received));
            Ok((request.into(), proxies, agent))
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

    send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": initialize,
    }));
    let initialize_response = recv().await;
    send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": { "cwd": "/v1-equivalence", "mcpServers": [], "_unknownRequestField": true },
    }));
    let session_response = recv().await;

    drop(incoming);
    tokio::time::timeout(TIMEOUT, task)
        .await
        .expect("conductor should finish after input EOF")
        .expect("conductor task should not panic")
        .expect("conductor should finish cleanly");

    let received = received.lock().expect("received lock").clone();
    let instantiated_version = *instantiated_version.lock().expect("version lock");
    ChainRun {
        responses: vec![initialize_response, session_response],
        received,
        instantiated_version,
    }
}

fn expected_initialize_response() -> Value {
    let mut response = serde_json::to_value(v1::InitializeResponse::new(ProtocolVersion::V1))
        .expect("serialize initialize response");
    response["_unknownResponseField"] = json!(true);
    response
}

fn assert_session_forwarded_unchanged(run: &ChainRun) {
    assert_eq!(run.responses[1]["id"], 2, "{}", run.responses[1]);
    assert_eq!(
        run.responses[1]["result"],
        json!({ "sessionId": "v1-session", "_unknownResponseField": true }),
        "the chain returns the agent's session/new response unchanged"
    );
    assert_eq!(
        run.received[1],
        (
            "session/new".to_string(),
            json!({ "cwd": "/v1-equivalence", "mcpServers": [], "_unknownRequestField": true }),
        ),
        "the agent receives the client's session/new request unchanged"
    );
}

/// A v1 `initialize` reaches the agent through a v1 proxy unchanged: the
/// conductor renames it to `_proxy/initialize` for the proxy without touching
/// its params, so unknown fields survive in both directions.
#[tokio::test(flavor = "current_thread")]
async fn v1_chain_forwards_initialize_unchanged() {
    let initialize = initialize_params(1);
    let run = run_v1_chain(&initialize).await;

    assert_eq!(run.instantiated_version, Some(ProtocolVersion::V1));
    assert_eq!(run.responses[0]["id"], 1, "{}", run.responses[0]);
    assert_eq!(
        run.responses[0]["result"],
        expected_initialize_response(),
        "the chain returns the agent's initialize response unchanged"
    );
    assert_eq!(
        run.received[0],
        ("initialize".to_string(), initialize),
        "unknown initialize fields survive the rewrite to `_proxy/initialize`"
    );
    assert_session_forwarded_unchanged(&run);
}

/// A request below v1 is not a v2 selection: the conductor keeps v1's
/// fallback-to-latest rule and forwards the v1 request it selected.
#[tokio::test(flavor = "current_thread")]
async fn v1_chain_falls_back_to_v1_below_v1() {
    let initialize = initialize_params(0);
    let run = run_v1_chain(&initialize).await;

    assert_eq!(
        run.instantiated_version,
        Some(ProtocolVersion::V1),
        "the instantiator sees the v1 selection"
    );
    assert_eq!(run.responses[0]["id"], 1, "{}", run.responses[0]);
    assert_eq!(run.responses[0]["result"], expected_initialize_response());
    let mut expected_initialize = initialize;
    expected_initialize["protocolVersion"] = json!(1);
    assert_eq!(
        run.received[0],
        (
            "initialize".to_string(),
            typed_value::<v1::InitializeRequest>(&expected_initialize),
        ),
        "the agent receives the selected v1 request"
    );
    assert_session_forwarded_unchanged(&run);
}

/// Without the `unstable_protocol_v2` feature, a request for a newer version
/// selects v1 and keeps only what parses under v1, as the SDK's v1 agents
/// negotiate.
#[cfg(not(feature = "unstable_protocol_v2"))]
#[tokio::test(flavor = "current_thread")]
async fn v1_chain_negotiates_newer_requests_to_v1() {
    let initialize = json!({
        "protocolVersion": 2,
        "info": { "name": "newer-client", "version": "1.0.0" },
        "capabilities": {},
        "_unknownRequestField": true,
    });
    let run = run_v1_chain(&initialize).await;

    assert_eq!(run.instantiated_version, Some(ProtocolVersion::V1));
    assert_eq!(run.responses[0]["result"], expected_initialize_response());
    let (method, received) = &run.received[0];
    assert_eq!(method, "initialize");
    assert_eq!(received["protocolVersion"], 1);
    assert_eq!(received["clientInfo"]["name"], "newer-client");
    assert!(received.get("_unknownRequestField").is_none(), "{received}");
    assert_session_forwarded_unchanged(&run);
}
