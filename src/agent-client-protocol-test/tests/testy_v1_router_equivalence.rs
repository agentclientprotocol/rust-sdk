//! The `testy` binary serves v1 and v2 through `testy::protocol_router` when
//! `unstable_protocol_v2` is enabled, and the v1 `Testy` alone otherwise. A v1
//! client must not be able to tell the two apart.

#![cfg(feature = "unstable_protocol_v2")]

use std::time::Duration;

use agent_client_protocol::{
    ByteStreams, Client, ConnectTo,
    schema::{ProtocolVersion, v1},
};
use agent_client_protocol_test::testy::{self, v1::Testy};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

const TIMEOUT: Duration = Duration::from_secs(5);

/// Send `messages` to `agent` one at a time and return what the agent wrote in
/// answer to the requests among them. A JSON string is written verbatim as a
/// raw line, so a test can send text that is not valid JSON; it and a JSON
/// array (a batch) each get one response.
async fn exchange(agent: impl ConnectTo<Client> + 'static, messages: &[Value]) -> Vec<Value> {
    let (mut client_writer, agent_reader) = tokio::io::duplex(64 * 1024);
    let (agent_writer, client_reader) = tokio::io::duplex(64 * 1024);
    let task = tokio::spawn(agent.connect_to(ByteStreams::new(
        agent_writer.compat_write(),
        agent_reader.compat(),
    )));
    let mut lines = BufReader::new(client_reader).lines();

    let mut written = Vec::new();
    for message in messages {
        let line = match message {
            Value::String(raw) => raw.clone(),
            message => message.to_string(),
        };
        client_writer
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("agent is still reading");
        if !message.is_string() && !message.is_array() && message.get("id").is_none() {
            continue;
        }
        let line = tokio::time::timeout(TIMEOUT, lines.next_line())
            .await
            .expect("agent should answer each request")
            .expect("agent output is readable")
            .expect("agent closed its output");
        written.push(serde_json::from_str(&line).expect("agent writes JSON"));
    }

    drop(client_writer);
    drop(lines);
    tokio::time::timeout(TIMEOUT, task)
        .await
        .expect("agent should finish after input EOF")
        .expect("agent task should not panic")
        .expect("agent should finish cleanly");
    written
}

fn initialize(params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": params })
}

fn v1_initialize_params() -> Value {
    serde_json::to_value(v1::InitializeRequest::new(ProtocolVersion::V1))
        .expect("serialize initialize")
}

fn new_session(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/new",
        "params": { "cwd": std::env::temp_dir(), "mcpServers": [] },
    })
}

async fn assert_same_for_v1_client(messages: &[Value]) {
    let standalone = exchange(Testy::new(), messages).await;
    let routed = exchange(testy::protocol_router(), messages).await;
    assert_eq!(
        routed, standalone,
        "the protocol router changed what a v1 client sees for {messages:#?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn v1_initialize_and_session() {
    assert_same_for_v1_client(&[initialize(v1_initialize_params()), new_session(2)]).await;
}

#[tokio::test(flavor = "current_thread")]
async fn request_below_v1_falls_back_to_v1() {
    let mut params = v1_initialize_params();
    params["protocolVersion"] = json!(0);
    assert_same_for_v1_client(&[initialize(params), new_session(2)]).await;
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_initialize() {
    assert_same_for_v1_client(&[initialize(json!({})), initialize(v1_initialize_params())]).await;
}

#[tokio::test(flavor = "current_thread")]
async fn request_before_initialize() {
    assert_same_for_v1_client(&[new_session(1), initialize(v1_initialize_params())]).await;
}

#[tokio::test(flavor = "current_thread")]
async fn notification_before_initialize() {
    assert_same_for_v1_client(&[
        json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": "unknown" },
        }),
        initialize(v1_initialize_params()),
    ])
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn batch_before_initialize() {
    assert_same_for_v1_client(&[
        json!([initialize(v1_initialize_params())]),
        initialize(v1_initialize_params()),
    ])
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_json_before_initialize() {
    assert_same_for_v1_client(&[json!("{"), initialize(v1_initialize_params())]).await;
}
