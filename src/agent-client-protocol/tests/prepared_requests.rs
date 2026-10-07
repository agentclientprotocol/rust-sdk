//! Exercise prepared requests from outside a concurrently driven connection.
//!
//! These workflows use the public API and real transport actors, with explicit
//! handshakes instead of timing assumptions about response or callback delivery.

use std::time::Duration;

use agent_client_protocol::{
    Channel, ConnectionTo, Error, RawJsonRpcMessage, TransportBatch, TransportFrame,
    UntypedMessage, role::UntypedRole,
};
use futures::{
    FutureExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
};
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_prepared_callback_holds_following_batch_entry_and_eof() {
    for result in [Ok(json!({"value": 42})), Err(Error::invalid_params())] {
        let ExternalConnection {
            driver,
            connection_rx,
            stop_tx,
            mut notifications,
            mut peer,
        } = external_connection();
        let (boundary_tx, boundary_rx) = oneshot::channel();
        let expected = result.clone();
        let caller = async move {
            let connection = connection_rx.await.unwrap();
            let prepared =
                connection.prepare_request(UntypedMessage::new("prepared", json!({})).unwrap());
            connection
                .send_notification(UntypedMessage::new("before-publication", json!({})).unwrap())
                .unwrap();
            boundary_rx.await.unwrap();

            let (started_tx, started_rx) = oneshot::channel();
            let (release_tx, release_rx) = oneshot::channel();
            prepared
                .on_receiving_result(async move |response| {
                    started_tx
                        .send(response)
                        .map_err(|_| Error::internal_error())?;
                    release_rx.await.map_err(Error::into_internal_error)?;
                    Ok(())
                })
                .unwrap();
            assert_eq!(started_rx.await.unwrap(), expected);
            assert!(notifications.next().now_or_never().is_none());
            assert!(!connection.is_incoming_closed());
            // This release comes from the external caller, not inbound traffic.
            release_tx.send(()).unwrap();
            assert_eq!(notifications.next().await.unwrap().method(), "following");
            connection.incoming_closed().await;
            stop_tx.send(()).unwrap();
        };
        let peer = async move {
            let Some(TransportFrame::Single(RawJsonRpcMessage::Notification(boundary))) =
                peer.rx.next().await
            else {
                panic!("preparation must not send before the notification");
            };
            assert_eq!(boundary.method.as_ref(), "before-publication");
            boundary_tx.send(()).unwrap();
            let Some(TransportFrame::Single(RawJsonRpcMessage::Request(request))) =
                peer.rx.next().await
            else {
                panic!("expected publication after selecting the callback");
            };
            assert_eq!(request.method.as_ref(), "prepared");
            peer.tx
                .unbounded_send(TransportFrame::Batch(
                    TransportBatch::from_messages([
                        RawJsonRpcMessage::response(request.id, result),
                        RawJsonRpcMessage::notification("following".into(), json!({})).unwrap(),
                    ])
                    .unwrap(),
                ))
                .unwrap();
            drop(peer.tx);
            assert!(
                peer.rx.next().await.is_none(),
                "completed request emitted another message"
            );
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            let ((), (), driver) = futures::join!(caller, peer, driver);
            driver.unwrap().unwrap();
        })
        .await
        .expect("ordered callback did not release the batch and EOF");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_blocking_request_publishes_before_polling_without_holding_dispatch() {
    let ExternalConnection {
        driver,
        connection_rx,
        stop_tx,
        mut notifications,
        mut peer,
    } = external_connection();
    let caller = async move {
        let connection = connection_rx.await.unwrap();
        let response = connection
            .prepare_request(UntypedMessage::new("prepared", json!({})).unwrap())
            .block_task();
        connection
            .send_notification(UntypedMessage::new("after-request", json!({})).unwrap())
            .unwrap();
        // Receive later traffic while the response future is still unpolled.
        assert_eq!(notifications.next().await.unwrap().method(), "following");
        assert_eq!(response.await.unwrap(), json!({"value": 42}));
        connection.incoming_closed().await;
        stop_tx.send(()).unwrap();
    };
    let peer = async move {
        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(request))) =
            peer.rx.next().await
        else {
            panic!("block_task must publish before polling");
        };
        assert_eq!(request.method.as_ref(), "prepared");
        let Some(TransportFrame::Single(RawJsonRpcMessage::Notification(notification))) =
            peer.rx.next().await
        else {
            panic!("expected notification after the request");
        };
        assert_eq!(notification.method.as_ref(), "after-request");
        peer.tx
            .unbounded_send(TransportFrame::Batch(
                TransportBatch::from_messages([
                    RawJsonRpcMessage::response(request.id, Ok(json!({"value": 42}))),
                    RawJsonRpcMessage::notification("following".into(), json!({})).unwrap(),
                ])
                .unwrap(),
            ))
            .unwrap();
        drop(peer.tx);
        assert!(
            peer.rx.next().await.is_none(),
            "completed request emitted another message"
        );
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        let ((), (), driver) = futures::join!(caller, peer, driver);
        driver.unwrap().unwrap();
    })
    .await
    .expect("unordered response consumption stalled dispatch");
}

struct ExternalConnection {
    driver: tokio::task::JoinHandle<Result<(), Error>>,
    connection_rx: oneshot::Receiver<ConnectionTo<UntypedRole>>,
    stop_tx: oneshot::Sender<()>,
    notifications: mpsc::UnboundedReceiver<UntypedMessage>,
    peer: Channel,
}

fn external_connection() -> ExternalConnection {
    let (transport, peer) = Channel::duplex();
    let (connection_tx, connection_rx) = oneshot::channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let (notification_tx, notifications) = mpsc::unbounded();
    let driver = tokio::spawn(
        UntypedRole
            .builder()
            .on_receive_notification(
                async move |notification: UntypedMessage,
                            _connection: ConnectionTo<UntypedRole>| {
                    notification_tx
                        .unbounded_send(notification)
                        .map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(transport, async move |connection| {
                connection_tx
                    .send(connection)
                    .map_err(|_| Error::internal_error())?;
                stop_rx.await.map_err(Error::into_internal_error)?;
                Ok(())
            }),
    );
    ExternalConnection {
        driver,
        connection_rx,
        stop_tx,
        notifications,
        peer,
    }
}
