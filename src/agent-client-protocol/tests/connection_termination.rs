//! Tests for the universal connection termination event.

use std::{future, io, time::Duration};

use agent_client_protocol::{
    Channel, ConnectionTermination, ConnectionTo, Error, Lines, RawJsonRpcMessage, TransportFrame,
    is_connection_terminated, is_incoming_transport_closed, role::UntypedRole,
};
use agent_client_protocol_test::MyRequest;
use futures::{StreamExt as _, channel::oneshot, stream};

const TIMEOUT: Duration = Duration::from_secs(2);

type Escaped = ConnectionTo<UntypedRole>;

async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TIMEOUT, future)
        .await
        .expect("operation timed out")
}

#[tokio::test]
async fn clean_eof_terminates_with_incoming_closed() {
    let (transport, peer) = Channel::duplex();
    let (escaped_tx, escaped_rx) = oneshot::channel::<Escaped>();

    let driver = UntypedRole
        .builder()
        .with_spawned(async move |cx| {
            escaped_tx.send(cx).expect("observer is alive");
            future::pending().await
        })
        .connect_to(transport);
    let observer = async {
        let cx = escaped_rx.await.expect("connection escaped");
        assert_eq!(cx.termination(), None);
        drop(peer);
        cx.terminated().await
    };

    let (result, termination) = within(futures::future::join(driver, observer)).await;
    result.expect("clean EOF is not an error");
    assert_eq!(termination, ConnectionTermination::IncomingClosed);
    assert!(termination.is_clean());
}

#[tokio::test]
async fn foreground_success_terminates_with_local_shutdown() {
    let (transport, _peer) = Channel::duplex();

    let cx = within(
        UntypedRole
            .builder()
            .connect_with(transport, async |cx| Ok(cx)),
    )
    .await
    .expect("foreground succeeds");

    assert_eq!(cx.termination(), Some(ConnectionTermination::LocalShutdown));
    assert_eq!(cx.terminated().await, ConnectionTermination::LocalShutdown);
}

#[tokio::test]
async fn foreground_error_is_reported() {
    let (transport, _peer) = Channel::duplex();
    let (escaped_tx, escaped_rx) = oneshot::channel::<Escaped>();

    let error = within(UntypedRole.builder().connect_with(transport, async |cx| {
        escaped_tx.send(cx).expect("observer is alive");
        Err::<(), _>(Error::invalid_params().data("foreground failed"))
    }))
    .await
    .expect_err("foreground error is returned");

    let cx = escaped_rx.await.expect("connection escaped");
    assert_eq!(
        cx.termination(),
        Some(ConnectionTermination::ForegroundError(error))
    );
}

#[tokio::test]
async fn runner_error_is_a_handler_error() {
    let (transport, _peer) = Channel::duplex();
    let (escaped_tx, escaped_rx) = oneshot::channel::<Escaped>();

    let error = within(
        UntypedRole
            .builder()
            .with_spawned(async move |cx| {
                escaped_tx.send(cx).expect("observer is alive");
                Err(Error::internal_error().data("runner failed"))
            })
            .connect_with(transport, async |_cx| {
                future::pending::<Result<(), Error>>().await
            }),
    )
    .await
    .expect_err("runner error is returned");

    let cx = escaped_rx.await.expect("connection escaped");
    assert_eq!(
        cx.terminated().await,
        ConnectionTermination::HandlerError(error)
    );
}

#[tokio::test]
async fn close_callback_error_is_a_handler_error() {
    let (transport, peer) = Channel::duplex();
    drop(peer);
    let (escaped_tx, escaped_rx) = oneshot::channel::<Escaped>();

    let error = within(
        UntypedRole
            .builder()
            .on_close(async move |cx| {
                escaped_tx.send(cx).expect("observer is alive");
                Err(Error::internal_error().data("close failed"))
            })
            .connect_with(transport, async |_cx| {
                future::pending::<Result<(), Error>>().await
            }),
    )
    .await
    .expect_err("close callback error is returned");

    let cx = escaped_rx.await.expect("connection escaped");
    assert_eq!(
        cx.termination(),
        Some(ConnectionTermination::HandlerError(error))
    );
}

#[tokio::test]
async fn transport_read_error_is_a_transport_error() {
    let outgoing = futures::sink::unfold((), |(), _line: String| async { Ok::<_, io::Error>(()) });
    let (escaped_tx, escaped_rx) = oneshot::channel::<Escaped>();
    let (fail_tx, fail_rx) = oneshot::channel::<()>();
    // Fail the read only after the foreground has escaped the connection.
    let incoming = stream::once(async {
        let _ = fail_rx.await;
        Err(io::Error::other("read failed"))
    })
    .chain(stream::pending());

    let error = within(UntypedRole.builder().connect_with(
        Lines::new(outgoing, incoming),
        async |cx| {
            escaped_tx.send(cx).expect("observer is alive");
            let _ = fail_tx.send(());
            future::pending::<Result<(), Error>>().await
        },
    ))
    .await
    .expect_err("read error is returned");

    let cx = escaped_rx.await.expect("connection escaped");
    assert_eq!(
        cx.termination(),
        Some(ConnectionTermination::TransportError(error))
    );
}

#[tokio::test]
async fn pending_requests_fail_with_termination_after_local_shutdown() {
    let (transport, mut peer) = Channel::duplex();
    let (escaped_tx, escaped_rx) = oneshot::channel::<Escaped>();

    // An escaped handle keeps a request alive past the foreground.
    let driver = UntypedRole.builder().connect_with(transport, async |cx| {
        escaped_tx.send(cx.clone()).expect("observer is alive");
        // Wait until the request reaches the peer, then finish.
        assert!(matches!(
            peer.rx.next().await,
            Some(TransportFrame::Single(RawJsonRpcMessage::Request(_)))
        ));
        Ok(())
    });
    let requester = async {
        let cx = escaped_rx.await.expect("connection escaped");
        let response = cx.send_request(MyRequest {}).block_task().await;
        (cx, response)
    };

    let (result, (cx, response)) = within(futures::future::join(driver, requester)).await;
    result.expect("foreground succeeds");

    let error = response.expect_err("request cannot be answered after termination");
    assert!(is_connection_terminated(&error), "{error:?}");
    assert!(!is_incoming_transport_closed(&error));
    let data = error.data.as_ref().expect("termination error data");
    assert_eq!(data["method"], "myRequest");
    assert_eq!(data["termination"], "local_shutdown");

    // Requests sent afterwards fail the same way.
    let late = cx
        .send_request(MyRequest {})
        .block_task()
        .await
        .expect_err("late request fails");
    assert!(is_connection_terminated(&late), "{late:?}");
}

#[tokio::test]
async fn pending_requests_keep_the_eof_error_after_clean_eof() {
    let (transport, mut peer) = Channel::duplex();

    let cx = within(UntypedRole.builder().connect_with(transport, async |cx| {
        let request = cx.send_request(MyRequest {});
        assert!(matches!(
            peer.rx.next().await,
            Some(TransportFrame::Single(RawJsonRpcMessage::Request(_)))
        ));
        drop(peer);
        let error = request
            .block_task()
            .await
            .expect_err("EOF fails pending requests");
        assert!(is_incoming_transport_closed(&error), "{error:?}");
        assert_eq!(cx.termination(), None, "foreground is still running");
        Ok(cx)
    }))
    .await
    .expect("foreground succeeds");

    assert_eq!(
        cx.termination(),
        Some(ConnectionTermination::IncomingClosed)
    );
}

#[tokio::test]
async fn dropping_the_driver_publishes_dropped() {
    let (transport, _peer) = Channel::duplex();
    let (escaped_tx, escaped_rx) = oneshot::channel::<Escaped>();

    let mut driver = Box::pin(UntypedRole.builder().connect_with(transport, async |cx| {
        escaped_tx.send(cx).expect("observer is alive");
        future::pending::<Result<(), Error>>().await
    }));
    // Poll once so the foreground starts and escapes the connection.
    assert!(futures::poll!(driver.as_mut()).is_pending());
    let cx = escaped_rx.await.expect("connection escaped");

    // A request awaiting a response when the driver is dropped.
    let request = cx.send_request(MyRequest {});
    assert!(futures::poll!(driver.as_mut()).is_pending());
    drop(driver);

    assert_eq!(cx.termination(), Some(ConnectionTermination::Dropped));
    let error = within(request.block_task())
        .await
        .expect_err("dropped driver fails pending requests");
    assert!(is_connection_terminated(&error), "{error:?}");
    assert_eq!(error.data.as_ref().unwrap()["termination"], "dropped");
}
