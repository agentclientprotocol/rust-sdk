//! A request whose [`Responder`] is dropped without responding is answered
//! with an automatic error, exactly once, unless the connection is shutting
//! down.
//!
//! The tests drive the connection from a raw [`Channel`] peer and use an
//! explicitly answered `ping` request as an ordering barrier: the dispatch
//! loop handles messages in order, so any response the SDK sends for an
//! earlier request is on the wire before the `ping` response.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::RawJsonRpcId;
use agent_client_protocol::{
    Channel, Error, JsonRpcRequest, RawJsonRpcMessage, Responder, TransportFrame, UntypedRole,
};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "drop_responder", response = Value)]
struct DropResponder {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "drop_then_error", response = Value)]
struct DropThenError {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "answer", response = Value)]
struct Answer {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "forward", response = Value)]
struct Forward {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "backend", response = Value)]
struct Backend {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "drop_when_cancelled", response = Value)]
struct DropWhenCancelled {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "stash", response = Value)]
struct Stash {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "hold_in_task", response = Value)]
struct HoldInTask {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "ping", response = Value)]
struct Ping {}

type Stashed = Arc<Mutex<Option<Responder<Value>>>>;

/// Start a connection whose handlers exercise each responder outcome, and
/// return the raw peer plus the driver task.
fn start(stashed: Stashed) -> (Channel, tokio::task::JoinHandle<Result<(), Error>>) {
    let (transport, peer) = Channel::duplex();
    let connection = UntypedRole
        .builder()
        .on_receive_request(
            async |_: DropResponder, responder: Responder<Value>, _cx| {
                drop(responder);
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_: DropThenError, responder: Responder<Value>, _cx| {
                drop(responder);
                Err::<(), _>(Error::invalid_params().data("handler error"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_: Answer, responder: Responder<Value>, _cx| {
                responder.respond(json!({ "answered": true }))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_: Forward, responder: Responder<Value>, cx| {
                cx.send_request(Backend {}).forward_response_to(responder)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_: DropWhenCancelled, responder: Responder<Value>, cx| {
                cx.spawn(async move {
                    responder.cancellation().cancelled().await;
                    drop(responder);
                    Ok(())
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let stashed = stashed.clone();
                async move |_: Stash, responder: Responder<Value>, _cx| {
                    *stashed.lock().unwrap() = Some(responder);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_: HoldInTask, responder: Responder<Value>, cx| {
                cx.spawn(async move {
                    let _responder = responder;
                    futures::future::pending::<()>().await;
                    Ok(())
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_: Ping, responder: Responder<Value>, _cx| responder.respond(json!("pong")),
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(transport);
    (peer, tokio::spawn(connection))
}

fn send_request(peer: &Channel, id: i64, method: &str) {
    peer.tx
        .unbounded_send(TransportFrame::Single(
            RawJsonRpcMessage::request(method.into(), json!({}), RawJsonRpcId::Number(id)).unwrap(),
        ))
        .unwrap();
}

fn send_cancel(peer: &Channel, id: i64) {
    peer.tx
        .unbounded_send(TransportFrame::Single(
            RawJsonRpcMessage::notification("$/cancel_request".into(), json!({ "requestId": id }))
                .unwrap(),
        ))
        .unwrap();
}

/// The next frame from the connection, as JSON.
async fn next_frame(peer: &mut Channel) -> Value {
    let frame = tokio::time::timeout(TIMEOUT, peer.rx.next())
        .await
        .expect("timed out waiting for a frame")
        .expect("connection closed before sending a frame");
    let TransportFrame::Single(message) = frame else {
        panic!("unexpected frame: {frame:?}");
    };
    serde_json::to_value(message).unwrap()
}

/// Send a `ping` and assert that the very next frame is its response, so the
/// connection sent nothing in between.
async fn assert_next_is_ping_response(peer: &mut Channel, id: i64) {
    send_request(peer, id, "ping");
    assert_eq!(
        next_frame(peer).await,
        json!({ "jsonrpc": "2.0", "id": id, "result": "pong" })
    );
}

fn assert_nothing_buffered(peer: &mut Channel) {
    if let Ok(frame) = peer.rx.try_recv() {
        panic!("unexpected extra frame: {frame:?}");
    }
}

fn error_code(response: &Value) -> i64 {
    response["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("expected an error response, got {response}"))
}

#[tokio::test]
async fn dropped_responder_is_answered_with_internal_error_once() {
    let (mut peer, driver) = start(Stashed::default());

    send_request(&peer, 1, "drop_responder");
    let response = next_frame(&mut peer).await;
    assert_eq!(response["id"], json!(1));
    assert_eq!(error_code(&response), -32603, "{response}");
    let data = response["error"]["data"].to_string();
    assert!(data.contains("drop_responder"), "{response}");

    assert_next_is_ping_response(&mut peer, 2).await;
    assert_next_is_ping_response(&mut peer, 3).await;
    assert_nothing_buffered(&mut peer);
    driver.abort();
}

#[tokio::test]
async fn handler_error_answers_instead_of_dropped_responder_fallback() {
    let (mut peer, driver) = start(Stashed::default());

    send_request(&peer, 1, "drop_then_error");
    let response = next_frame(&mut peer).await;
    assert_eq!(response["id"], json!(1));
    assert_eq!(error_code(&response), -32602, "{response}");

    assert_next_is_ping_response(&mut peer, 2).await;
    assert_nothing_buffered(&mut peer);
    driver.abort();
}

#[tokio::test]
async fn answered_responder_sends_nothing_extra() {
    let (mut peer, driver) = start(Stashed::default());

    send_request(&peer, 1, "answer");
    assert_eq!(
        next_frame(&mut peer).await,
        json!({ "jsonrpc": "2.0", "id": 1, "result": { "answered": true } })
    );

    assert_next_is_ping_response(&mut peer, 2).await;
    assert_nothing_buffered(&mut peer);
    driver.abort();
}

#[tokio::test]
async fn forwarded_response_sends_nothing_extra() {
    let (mut peer, driver) = start(Stashed::default());

    send_request(&peer, 1, "forward");
    let backend = next_frame(&mut peer).await;
    assert_eq!(backend["method"], json!("backend"), "{backend}");
    let backend_id: RawJsonRpcId = serde_json::from_value(backend["id"].clone()).unwrap();
    peer.tx
        .unbounded_send(TransportFrame::Single(RawJsonRpcMessage::response(
            backend_id,
            Ok(json!({ "forwarded": true })),
        )))
        .unwrap();

    assert_eq!(
        next_frame(&mut peer).await,
        json!({ "jsonrpc": "2.0", "id": 1, "result": { "forwarded": true } })
    );

    assert_next_is_ping_response(&mut peer, 2).await;
    assert_nothing_buffered(&mut peer);
    driver.abort();
}

#[tokio::test]
async fn cancelled_then_dropped_responder_is_answered_with_request_cancelled() {
    let (mut peer, driver) = start(Stashed::default());

    send_request(&peer, 1, "drop_when_cancelled");
    send_cancel(&peer, 1);
    let response = next_frame(&mut peer).await;
    assert_eq!(response["id"], json!(1));
    assert_eq!(error_code(&response), -32800, "{response}");

    assert_next_is_ping_response(&mut peer, 2).await;
    assert_nothing_buffered(&mut peer);
    driver.abort();
}

#[tokio::test]
async fn responder_dropped_after_connection_close_sends_nothing() {
    let stashed = Stashed::default();
    let (mut peer, driver) = start(stashed.clone());

    send_request(&peer, 1, "stash");
    send_request(&peer, 2, "hold_in_task");
    assert_next_is_ping_response(&mut peer, 3).await;
    assert!(stashed.lock().unwrap().is_some());

    // Incoming EOF shuts the connection down. The task holding the
    // `hold_in_task` responder is dropped during shutdown.
    peer.tx.close_channel();
    tokio::time::timeout(TIMEOUT, driver)
        .await
        .expect("connection did not shut down")
        .unwrap()
        .unwrap();

    // Dropping a responder after the connection has terminated must neither
    // panic nor send anything.
    drop(stashed.lock().unwrap().take());

    let remaining: Vec<_> = tokio::time::timeout(TIMEOUT, peer.rx.collect::<Vec<_>>())
        .await
        .expect("connection output did not close");
    assert!(remaining.is_empty(), "unexpected frames: {remaining:?}");
}
