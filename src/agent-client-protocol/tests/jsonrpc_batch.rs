//! JSON-RPC batch arrays are not supported.
//!
//! Every connection answers an incoming JSON array, including an empty one or
//! one holding only responses, with a single invalid-request (`-32600`) error
//! with `id: null`. None of the array's entries is dispatched, and the
//! connection stays usable.

use std::time::Duration;

use agent_client_protocol::{
    Agent, ByteStreams, ConnectTo, ConnectionTo, Error, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResponse, Responder, role::UntypedRole, schema::ProtocolVersion, schema::v1,
};
use futures::channel::mpsc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "test/echo", response = TestResponse)]
struct TestRequest {
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
struct TestResponse {
    result: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcNotification)]
#[notification(method = "test/notify")]
struct TestNotification {
    message: String,
}

struct Peer {
    writer: DuplexStream,
    reader: BufReader<DuplexStream>,
}

impl Peer {
    fn connect(component: impl ConnectTo<agent_client_protocol::Client> + 'static) -> Self {
        let (writer, sdk_reader) = tokio::io::duplex(8192);
        let (sdk_writer, reader) = tokio::io::duplex(8192);
        let transport = ByteStreams::new(sdk_writer.compat_write(), sdk_reader.compat());
        tokio::spawn(component.connect_to(transport));
        Self {
            writer,
            reader: BufReader::new(reader),
        }
    }

    fn connect_untyped(component: impl ConnectTo<UntypedRole> + 'static) -> Self {
        let (writer, sdk_reader) = tokio::io::duplex(8192);
        let (sdk_writer, reader) = tokio::io::duplex(8192);
        let transport = ByteStreams::new(sdk_writer.compat_write(), sdk_reader.compat());
        tokio::spawn(component.connect_to(transport));
        Self {
            writer,
            reader: BufReader::new(reader),
        }
    }

    async fn send(&mut self, value: &Value) {
        let mut bytes = serde_json::to_vec(value).expect("test JSON should serialize");
        bytes.push(b'\n');
        self.writer
            .write_all(&bytes)
            .await
            .expect("failed to write JSON-RPC line");
        self.writer
            .flush()
            .await
            .expect("failed to flush JSON-RPC line");
    }

    async fn receive(&mut self) -> Value {
        let mut line = String::new();
        let bytes_read = tokio::time::timeout(TIMEOUT, self.reader.read_line(&mut line))
            .await
            .expect("timed out waiting for JSON-RPC line")
            .expect("failed to read JSON-RPC line");
        assert_ne!(bytes_read, 0, "stream closed before expected JSON-RPC line");
        serde_json::from_str(line.trim()).expect("response should be valid JSON")
    }

    async fn exchange(&mut self, value: &Value) -> Value {
        self.send(value).await;
        self.receive().await
    }
}

fn assert_batch_rejected(response: &Value) {
    assert!(
        response.is_object(),
        "expected one response, got {response}"
    );
    assert_eq!(response["id"], Value::Null, "{response}");
    assert_eq!(response["error"]["code"], -32600, "{response}");
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn echo(id: u64, message: &str) -> Value {
    request(id, "test/echo", json!({ "message": message }))
}

#[tokio::test(flavor = "current_thread")]
async fn every_batch_shape_gets_one_invalid_request_error() {
    let (notification_tx, mut notifications) = mpsc::unbounded();
    let server = UntypedRole
        .builder()
        .on_receive_request(
            async |request: TestRequest,
                   responder: Responder<TestResponse>,
                   _connection: ConnectionTo<UntypedRole>| {
                responder.respond(TestResponse {
                    result: format!("echo: {}", request.message),
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: TestNotification, _connection: ConnectionTo<UntypedRole>| {
                notification_tx
                    .unbounded_send(notification.message)
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        );
    let mut peer = Peer::connect_untyped(server);

    let notification = json!({
        "jsonrpc": "2.0",
        "method": "test/notify",
        "params": { "message": "inside a batch" },
    });
    for batch in [
        json!([]),
        json!([1, 2, 3]),
        json!([echo(1, "one"), echo(2, "two")]),
        json!([notification]),
        json!([{ "jsonrpc": "2.0", "id": 7, "result": null }]),
    ] {
        assert_batch_rejected(&peer.exchange(&batch).await);
    }

    // No entry was dispatched, and the connection keeps serving individual
    // messages.
    let response = peer.exchange(&echo(3, "after batches")).await;
    assert_eq!(response["id"], 3, "{response}");
    assert_eq!(response["result"]["result"], "echo: after batches");
    assert!(notifications.try_recv().is_err(), "no notification ran");
}

#[tokio::test(flavor = "current_thread")]
async fn v1_agent_rejects_a_batched_initialize_and_accepts_a_retry() {
    let agent = Agent.builder().on_receive_request(
        async |initialize: v1::InitializeRequest, responder, _cx| {
            responder.respond(v1::InitializeResponse::new(initialize.protocol_version))
        },
        agent_client_protocol::on_receive_request!(),
    );
    let mut peer = Peer::connect(agent);

    let initialize = request(
        1,
        "initialize",
        serde_json::to_value(v1::InitializeRequest::new(ProtocolVersion::V1)).unwrap(),
    );
    assert_batch_rejected(&peer.exchange(&json!([initialize])).await);

    let response = peer.exchange(&initialize).await;
    assert_eq!(response["id"], 1, "{response}");
    assert_eq!(response["result"]["protocolVersion"], 1, "{response}");
}

#[cfg(feature = "unstable_protocol_v2")]
mod v2 {
    use agent_client_protocol::schema::v2;

    use super::*;

    fn initialize_params() -> Value {
        serde_json::to_value(v2::InitializeRequest::new(
            ProtocolVersion::V2,
            v2::Implementation::new("batch-test", "0.1.0"),
        ))
        .unwrap()
    }

    fn v2_agent() -> impl ConnectTo<agent_client_protocol::Client> {
        Agent.v2().on_receive_request(
            async |initialize: v2::InitializeRequest, responder, _cx| {
                responder.respond(v2::InitializeResponse::new(
                    initialize.protocol_version,
                    v2::Implementation::new("batch-agent", "0.1.0"),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn v2_agent_rejects_batches_before_and_after_initialization() {
        let mut peer = Peer::connect(v2_agent());
        let initialize = request(1, "initialize", initialize_params());

        assert_batch_rejected(&peer.exchange(&json!([initialize])).await);
        let response = peer.exchange(&initialize).await;
        assert_eq!(response["result"]["protocolVersion"], 2, "{response}");

        assert_batch_rejected(
            &peer
                .exchange(&json!([request(
                    2,
                    "session/new",
                    json!({ "cwd": "/tmp" })
                )]))
                .await,
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn protocol_router_rejects_a_batched_first_initialize_and_accepts_a_retry() {
        for protocol_version in [ProtocolVersion::V1, ProtocolVersion::V2] {
            let router = Agent
                .protocol_router()
                .with_v1(Agent.builder().on_receive_request(
                    async |initialize: v1::InitializeRequest, responder, _cx| {
                        responder.respond(v1::InitializeResponse::new(initialize.protocol_version))
                    },
                    agent_client_protocol::on_receive_request!(),
                ))
                .with_v2(v2_agent());
            let mut peer = Peer::connect(router);
            let params = if protocol_version == ProtocolVersion::V1 {
                serde_json::to_value(v1::InitializeRequest::new(ProtocolVersion::V1)).unwrap()
            } else {
                initialize_params()
            };
            let initialize = request(1, "initialize", params);

            assert_batch_rejected(&peer.exchange(&json!([initialize])).await);
            let response = peer.exchange(&initialize).await;
            assert_eq!(response["id"], 1, "{response}");
            assert_eq!(
                response["result"]["protocolVersion"],
                json!(protocol_version),
                "{response}"
            );
        }
    }
}
