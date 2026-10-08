//! Exercise prepared requests from outside a concurrently driven connection.
//!
//! These workflows use the public API and real transport actors, with explicit
//! handshakes instead of timing assumptions about response or callback delivery.

use std::time::Duration;

use agent_client_protocol::{
    Channel, ConnectionTo, Error, RawJsonRpcMessage, RequestCancellationHandle, TransportBatch,
    TransportFrame, UntypedMessage, role::UntypedRole,
};
use futures::{
    FutureExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
};
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_prepared_callback_holds_following_batch_entry_and_eof() {
    for result in [
        Ok(json!({"value": 42})),
        Err(Error::invalid_params()),
        Err(Error::request_cancelled()),
    ] {
        let ExternalConnection {
            driver,
            connection_rx,
            stop_tx,
            mut notifications,
            mut peer,
        } = external_connection();
        let (boundary_tx, boundary_rx) = oneshot::channel();
        let (routed_tx, routed_rx) = oneshot::channel();
        let expected = result.clone();
        let caller = async move {
            let connection = connection_rx.await.unwrap();
            let prepared =
                connection.prepare_request(UntypedMessage::new("prepared", json!({})).unwrap());
            let cancellation = prepared.cancellation_handle();
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
            cancellation.cancel().unwrap();
            cancellation.clone().cancel().unwrap();
            connection
                .send_notification(UntypedMessage::new("after-routed-response", json!({})).unwrap())
                .unwrap();
            routed_rx.await.unwrap();
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
            let Some(TransportFrame::Single(RawJsonRpcMessage::Notification(notification))) =
                peer.rx.next().await
            else {
                panic!("expected the routed-response barrier");
            };
            assert_eq!(
                notification.method.as_ref(),
                "after-routed-response",
                "cancellation must be disarmed while the ordered callback is held"
            );
            routed_tx.send(()).unwrap();
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
async fn prepared_cancellation_handle_survives_publication_and_preserves_callback_result() {
    fn assert_handle_traits<T: Clone + std::fmt::Debug + Send + Sync>() {}
    assert_handle_traits::<RequestCancellationHandle>();

    for result in [
        Ok(json!({"value": 42})),
        Err(Error::invalid_params()),
        Err(Error::request_cancelled()),
    ] {
        let ExternalConnection {
            driver,
            connection_rx,
            stop_tx,
            mut notifications,
            mut peer,
        } = external_connection();
        let (boundary_tx, boundary_rx) = oneshot::channel();
        let (published_tx, published_rx) = oneshot::channel();
        let expected = result.clone();
        let caller = async move {
            let connection = connection_rx.await.unwrap();
            let prepared =
                connection.prepare_request(UntypedMessage::new("prepared", json!({})).unwrap());
            let request_id = prepared.id().clone();
            let cancellation: RequestCancellationHandle = prepared.cancellation_handle();
            let cloned_cancellation = cancellation.clone();
            cancellation.cancel().unwrap();
            cloned_cancellation.cancel().unwrap();
            connection
                .send_notification(UntypedMessage::new("before-publication", json!({})).unwrap())
                .unwrap();
            boundary_rx.await.unwrap();

            let (callback_tx, callback_rx) = oneshot::channel();
            prepared
                .on_receiving_result(async move |response| {
                    callback_tx
                        .send(response)
                        .map_err(|_| Error::internal_error())
                })
                .unwrap();
            assert_eq!(published_rx.await.unwrap(), request_id);
            // A prepublication cancel must neither emit traffic nor consume
            // the once-only cancellation available after publication.
            cloned_cancellation.cancel().unwrap();
            cancellation.cancel().unwrap();
            connection
                .send_notification(UntypedMessage::new("after-cancellation", json!({})).unwrap())
                .unwrap();
            assert_eq!(callback_rx.await.unwrap(), expected);
            assert_eq!(notifications.next().await.unwrap().method(), "following");
            connection.incoming_closed().await;
            stop_tx.send(()).unwrap();
        };
        let peer = async move {
            let Some(TransportFrame::Single(RawJsonRpcMessage::Notification(boundary))) =
                peer.rx.next().await
            else {
                panic!("prepared cancellation must not publish the request");
            };
            assert_eq!(boundary.method.as_ref(), "before-publication");
            boundary_tx.send(()).unwrap();
            let Some(TransportFrame::Single(RawJsonRpcMessage::Request(request))) =
                peer.rx.next().await
            else {
                panic!("expected the prepared request after callback registration");
            };
            assert_eq!(request.method.as_ref(), "prepared");
            published_tx.send(request.id.clone()).unwrap();
            let Some(TransportFrame::Single(RawJsonRpcMessage::Notification(cancellation))) =
                peer.rx.next().await
            else {
                panic!("expected cancellation after publication");
            };
            assert_eq!(cancellation.method.as_ref(), "$/cancel_request");
            assert_eq!(
                serde_json::to_value(cancellation.params).unwrap(),
                json!({"requestId": request.id})
            );
            let Some(TransportFrame::Single(RawJsonRpcMessage::Notification(boundary))) =
                peer.rx.next().await
            else {
                panic!("expected the cancellation barrier");
            };
            assert_eq!(boundary.method.as_ref(), "after-cancellation");
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
                "cloned handles or callback completion emitted another cancellation"
            );
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            let ((), (), driver) = futures::join!(caller, peer, driver);
            driver.unwrap().unwrap();
        })
        .await
        .expect("retained cancellation handle lost the ordered callback result");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detached_requests_retain_explicit_cancellation_control() {
    for prepare in [false, true] {
        for result in [Ok(json!({"value": 42})), Err(Error::request_cancelled())] {
            let ExternalConnection {
                driver,
                connection_rx,
                stop_tx,
                mut notifications,
                mut peer,
            } = external_connection();
            let (detached_tx, detached_rx) = oneshot::channel();
            let caller = async move {
                let connection = connection_rx.await.unwrap();
                let application_owner = std::sync::Arc::new(());
                let application_owner_weak = std::sync::Arc::downgrade(&application_owner);
                let (request_id, cancellation) = if prepare {
                    let request = connection
                        .prepare_request(UntypedMessage::new("detached", json!({})).unwrap())
                        .map(move |response| {
                            drop(application_owner);
                            Ok(response)
                        });
                    let request_id = request.id().clone();
                    let cancellation = request.cancellation_handle();
                    request.detach().unwrap();
                    (request_id, cancellation)
                } else {
                    let request = connection
                        .send_request(UntypedMessage::new("detached", json!({})).unwrap())
                        .map(move |response| {
                            drop(application_owner);
                            Ok(response)
                        });
                    let request_id = request.id().clone();
                    let cancellation = request.cancellation_handle();
                    request.detach();
                    (request_id, cancellation)
                };
                assert!(
                    application_owner_weak.upgrade().is_none(),
                    "a retained cancellation handle must not retain the detached response consumer"
                );
                connection
                    .send_notification(UntypedMessage::new("after-detach", json!({})).unwrap())
                    .unwrap();
                assert_eq!(detached_rx.await.unwrap(), request_id);
                let cloned_cancellation = cancellation.clone();
                cancellation.cancel().unwrap();
                cloned_cancellation.cancel().unwrap();
                connection
                    .send_notification(
                        UntypedMessage::new("after-cancellation", json!({})).unwrap(),
                    )
                    .unwrap();

                // The following notification is dispatched after routing the
                // response, even though detach discarded its consumer.
                assert_eq!(notifications.next().await.unwrap().method(), "following");
                cancellation.cancel().unwrap();
                cloned_cancellation.cancel().unwrap();
                drop(cloned_cancellation);
                drop(cancellation);
                connection
                    .send_notification(
                        UntypedMessage::new("after-routed-response", json!({})).unwrap(),
                    )
                    .unwrap();
                connection.incoming_closed().await;
                stop_tx.send(()).unwrap();
            };
            let peer = async move {
                let Some(TransportFrame::Single(RawJsonRpcMessage::Request(request))) =
                    peer.rx.next().await
                else {
                    panic!("expected the detached request");
                };
                assert_eq!(request.method.as_ref(), "detached");
                let Some(TransportFrame::Single(RawJsonRpcMessage::Notification(boundary))) =
                    peer.rx.next().await
                else {
                    panic!("expected the detach barrier");
                };
                assert_eq!(
                    boundary.method.as_ref(),
                    "after-detach",
                    "detach must not emit automatic cancellation"
                );
                detached_tx.send(request.id.clone()).unwrap();
                let Some(TransportFrame::Single(message)) = peer.rx.next().await else {
                    panic!("expected explicit cancellation after detach");
                };
                assert!(
                    serde_json::to_value(&message).unwrap().get("id").is_none(),
                    "cancellation must have no outer request id"
                );
                let RawJsonRpcMessage::Notification(cancellation) = message else {
                    panic!("cancellation must be a notification");
                };
                assert_eq!(cancellation.method.as_ref(), "$/cancel_request");
                assert_eq!(
                    serde_json::to_value(cancellation.params).unwrap(),
                    json!({"requestId": request.id})
                );
                let Some(TransportFrame::Single(RawJsonRpcMessage::Notification(boundary))) =
                    peer.rx.next().await
                else {
                    panic!("expected the cancellation barrier");
                };
                assert_eq!(
                    boundary.method.as_ref(),
                    "after-cancellation",
                    "cloned handles must emit exactly one cancellation"
                );
                peer.tx
                    .unbounded_send(TransportFrame::Batch(
                        TransportBatch::from_messages([
                            RawJsonRpcMessage::response(request.id, result),
                            RawJsonRpcMessage::notification("following".into(), json!({})).unwrap(),
                        ])
                        .unwrap(),
                    ))
                    .unwrap();
                let Some(TransportFrame::Single(RawJsonRpcMessage::Notification(boundary))) =
                    peer.rx.next().await
                else {
                    panic!("expected the routed-response barrier");
                };
                assert_eq!(
                    boundary.method.as_ref(),
                    "after-routed-response",
                    "cancelling after response routing must not emit another cancellation"
                );
                drop(peer.tx);
                assert!(
                    peer.rx.next().await.is_none(),
                    "detached request or retained handles emitted another message"
                );
            };
            tokio::time::timeout(Duration::from_secs(10), async {
                let ((), (), driver) = futures::join!(caller, peer, driver);
                driver.unwrap().unwrap();
            })
            .await
            .expect("detached request lost independent cancellation control");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_handles_share_once_only_state_with_sent_request_and_drop() {
    let ExternalConnection {
        driver,
        connection_rx,
        stop_tx,
        notifications: _,
        mut peer,
    } = external_connection();
    let caller = async move {
        let connection = connection_rx.await.unwrap();
        let request = connection.send_request(UntypedMessage::new("pending", json!({})).unwrap());
        let cancellation: RequestCancellationHandle = request.cancellation_handle();
        let cloned_cancellation = cancellation.clone();
        cancellation.cancel().unwrap();
        request.cancel().unwrap();
        cloned_cancellation.cancel().unwrap();
        drop(request);
        cancellation.cancel().unwrap();
        drop(cloned_cancellation);
        drop(cancellation);
        connection
            .send_notification(UntypedMessage::new("after-drop", json!({})).unwrap())
            .unwrap();
        connection.incoming_closed().await;
        stop_tx.send(()).unwrap();
    };
    let peer = async move {
        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(request))) =
            peer.rx.next().await
        else {
            panic!("expected the pending request");
        };
        assert_eq!(request.method.as_ref(), "pending");
        let Some(TransportFrame::Single(RawJsonRpcMessage::Notification(cancellation))) =
            peer.rx.next().await
        else {
            panic!("expected one cancellation");
        };
        assert_eq!(cancellation.method.as_ref(), "$/cancel_request");
        assert_eq!(
            serde_json::to_value(cancellation.params).unwrap(),
            json!({"requestId": request.id})
        );
        let Some(TransportFrame::Single(RawJsonRpcMessage::Notification(boundary))) =
            peer.rx.next().await
        else {
            panic!("expected the drop barrier");
        };
        assert_eq!(boundary.method.as_ref(), "after-drop");
        drop(peer.tx);
        assert!(
            peer.rx.next().await.is_none(),
            "explicit cancellation or drop emitted a duplicate"
        );
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        let ((), (), driver) = futures::join!(caller, peer, driver);
        driver.unwrap().unwrap();
    })
    .await
    .expect("shared cancellation did not reach the wire");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_cancellation_handles_leaves_request_drop_armed() {
    let ExternalConnection {
        driver,
        connection_rx,
        stop_tx,
        notifications: _,
        mut peer,
    } = external_connection();
    let (boundary_tx, boundary_rx) = oneshot::channel();
    let (dropped_tx, dropped_rx) = oneshot::channel();
    let caller = async move {
        let connection = connection_rx.await.unwrap();
        let application_owner = std::sync::Arc::new(());
        let application_owner_weak = std::sync::Arc::downgrade(&application_owner);
        let request = connection
            .send_request(UntypedMessage::new("pending", json!({})).unwrap())
            .map(move |response| {
                drop(application_owner);
                Ok(response)
            });
        let cancellation = request.cancellation_handle();
        let cloned_cancellation = cancellation.clone();
        drop(cancellation);
        drop(cloned_cancellation);
        connection
            .send_notification(UntypedMessage::new("handle-dropped", json!({})).unwrap())
            .unwrap();
        boundary_rx.await.unwrap();
        let retained_cancellation = request.cancellation_handle();
        drop(request);
        assert!(
            application_owner_weak.upgrade().is_none(),
            "a cancellation handle must not retain the response consumer's captured owner"
        );
        dropped_rx.await.unwrap();
        retained_cancellation.cancel().unwrap();
        connection
            .send_notification(UntypedMessage::new("request-dropped", json!({})).unwrap())
            .unwrap();
        connection.incoming_closed().await;
        drop(retained_cancellation);
        stop_tx.send(()).unwrap();
    };
    let peer = async move {
        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(request))) =
            peer.rx.next().await
        else {
            panic!("expected the pending request");
        };
        assert_eq!(request.method.as_ref(), "pending");
        let Some(TransportFrame::Single(RawJsonRpcMessage::Notification(boundary))) =
            peer.rx.next().await
        else {
            panic!("expected the handle-drop barrier");
        };
        assert_eq!(
            boundary.method.as_ref(),
            "handle-dropped",
            "dropping a cancellation handle must not send cancellation"
        );
        boundary_tx.send(()).unwrap();
        let Some(TransportFrame::Single(RawJsonRpcMessage::Notification(cancellation))) =
            peer.rx.next().await
        else {
            panic!("request drop must cancel with a cancellation handle still retained");
        };
        assert_eq!(cancellation.method.as_ref(), "$/cancel_request");
        assert_eq!(
            serde_json::to_value(cancellation.params).unwrap(),
            json!({"requestId": request.id})
        );
        dropped_tx.send(()).unwrap();
        let Some(TransportFrame::Single(RawJsonRpcMessage::Notification(boundary))) =
            peer.rx.next().await
        else {
            panic!("expected the request-drop barrier");
        };
        assert_eq!(boundary.method.as_ref(), "request-dropped");
        drop(peer.tx);
        assert!(
            peer.rx.next().await.is_none(),
            "dropping the retained cancellation handle emitted another message"
        );
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        let ((), (), driver) = futures::join!(caller, peer, driver);
        driver.unwrap().unwrap();
    })
    .await
    .expect("cancellation handle changed automatic request-drop cancellation");
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
