//! Request IDs are scoped to the direction that issued them.
//!
//! Each side picks its own request IDs, so the same ID can be in flight in
//! both directions at once. Responses and `$/cancel_request` must be matched
//! against the requests of the side that issued them.

use std::time::Duration;

use agent_client_protocol::{
    ByteStreams, ConnectionTo, Error, JsonRpcRequest, JsonRpcResponse, Responder, role::UntypedRole,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "test/ask", response = Answer)]
struct Ask {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "test/question", response = Answer)]
struct Question {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "test/slow", response = Answer)]
struct Slow {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
struct Answer {
    answer: String,
}

async fn send(writer: &mut DuplexStream, value: &Value) {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    writer.write_all(&bytes).await.unwrap();
    writer.flush().await.unwrap();
}

async fn receive(reader: &mut BufReader<DuplexStream>) -> Value {
    let mut line = String::new();
    tokio::time::timeout(TIMEOUT, reader.read_line(&mut line))
        .await
        .expect("timed out waiting for a JSON-RPC line")
        .unwrap();
    serde_json::from_str(line.trim()).unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn the_same_request_id_can_be_in_flight_in_both_directions() {
    let server = UntypedRole
        .builder()
        .on_receive_request(
            async |_ask: Ask, responder: Responder<Answer>, cx: ConnectionTo<UntypedRole>| {
                // Ask the peer a question and answer `test/ask` with its reply.
                cx.send_request(Question {})
                    .on_receiving_result(async move |result| responder.respond_with_result(result))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_slow: Slow, responder: Responder<Answer>, cx: ConnectionTo<UntypedRole>| {
                let cancellation = responder.cancellation();
                cx.spawn(async move {
                    cancellation.cancelled().await;
                    responder.respond_with_error(Error::request_cancelled())
                })
            },
            agent_client_protocol::on_receive_request!(),
        );

    let (mut writer, sdk_reader) = tokio::io::duplex(8192);
    let (sdk_writer, reader) = tokio::io::duplex(8192);
    let transport = ByteStreams::new(sdk_writer.compat_write(), sdk_reader.compat());
    let server_task = tokio::spawn(server.connect_to(transport));
    let mut reader = BufReader::new(reader);

    send(
        &mut writer,
        &json!({ "jsonrpc": "2.0", "id": "ask", "method": "test/ask", "params": {} }),
    )
    .await;
    let question = receive(&mut reader).await;
    assert_eq!(question["method"], "test/question", "{question}");
    let shared_id = question["id"].clone();

    // The peer issues its own request with the ID the SDK chose for its
    // pending question, then cancels it. The cancellation names the peer's
    // request, so only `test/slow` is cancelled.
    send(
        &mut writer,
        &json!({ "jsonrpc": "2.0", "id": shared_id, "method": "test/slow", "params": {} }),
    )
    .await;
    send(
        &mut writer,
        &json!({
            "jsonrpc": "2.0",
            "method": "$/cancel_request",
            "params": { "requestId": shared_id },
        }),
    )
    .await;
    let cancelled = receive(&mut reader).await;
    assert_eq!(cancelled["id"], shared_id, "{cancelled}");
    assert_eq!(cancelled["error"]["code"], -32800, "{cancelled}");

    // The SDK's own request with the same ID is still pending and accepts
    // the peer's answer.
    send(
        &mut writer,
        &json!({ "jsonrpc": "2.0", "id": shared_id, "result": { "answer": "42" } }),
    )
    .await;
    let answered = receive(&mut reader).await;
    assert_eq!(answered["id"], "ask", "{answered}");
    assert_eq!(answered["result"]["answer"], "42", "{answered}");

    drop(writer);
    tokio::time::timeout(TIMEOUT, server_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
