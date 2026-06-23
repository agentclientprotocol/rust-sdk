//! Tests for transport close (EOF) detection.
//!
//! Verifies that `connect_with` and `connect_to` return when the remote
//! end of the transport closes, rather than hanging forever.

use agent_client_protocol::{ByteStreams, Dispatch, Handled, role::UntypedRole};
use futures::{AsyncRead, AsyncWrite};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

fn setup_test_streams() -> (
    impl AsyncRead,
    impl AsyncWrite,
    impl AsyncRead,
    impl AsyncWrite,
) {
    let (client_writer, server_reader) = tokio::io::duplex(1024);
    let (server_writer, client_reader) = tokio::io::duplex(1024);

    (
        server_reader.compat(),
        server_writer.compat_write(),
        client_reader.compat(),
        client_writer.compat_write(),
    )
}

/// When the remote side's `connect_with` returns (dropping the transport),
/// the local `connect_with` with a blocking main_fn should also return
/// rather than hanging forever.
#[tokio::test(flavor = "current_thread")]
async fn connect_with_returns_on_remote_transport_close() {
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local
        .run_until(async {
            let (server_reader, server_writer, client_reader, client_writer) = setup_test_streams();

            let server_transport = ByteStreams::new(server_writer, server_reader);
            let client_transport = ByteStreams::new(client_writer, client_reader);

            // Server accepts and immediately returns (closing transport).
            tokio::task::spawn_local(async move {
                UntypedRole
                    .builder()
                    .name("server")
                    .connect_with(server_transport, async move |_cx| Ok(()))
                    .await
                    .ok();
            });

            // Client blocks on an mpsc channel (simulating a proxy pattern).
            let (_tx, mut rx) = mpsc::unbounded_channel::<Dispatch>();
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                UntypedRole.builder().name("client").connect_with(
                    client_transport,
                    async move |_cx| {
                        // Block until channel closes — but transport EOF should
                        // cancel this future before that happens.
                        while rx.recv().await.is_some() {}
                        Ok(())
                    },
                ),
            )
            .await;

            assert!(
                result.is_ok(),
                "connect_with should return on transport close, not time out"
            );
            // The connection closed before main_fn finished, so it returns Err.
            assert!(result.unwrap().is_err());
        })
        .await;
}

/// `connect_to` (server mode) should return when the remote end disconnects.
#[tokio::test(flavor = "current_thread")]
async fn connect_to_returns_on_remote_transport_close() {
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local
        .run_until(async {
            let (server_reader, server_writer, client_reader, client_writer) = setup_test_streams();

            let server_transport = ByteStreams::new(server_writer, server_reader);
            let client_transport = ByteStreams::new(client_writer, client_reader);

            // Client connects and immediately returns.
            tokio::task::spawn_local(async move {
                UntypedRole
                    .builder()
                    .name("client")
                    .connect_with(client_transport, async move |_cx| Ok(()))
                    .await
                    .ok();
            });

            // Server uses connect_to (pending foreground) — should detect EOF.
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                UntypedRole
                    .builder()
                    .name("server")
                    .connect_to(server_transport),
            )
            .await;

            assert!(
                result.is_ok(),
                "connect_to should return on transport close, not time out"
            );
        })
        .await;
}

/// When using `on_receive_dispatch` with a forwarding channel pattern,
/// transport close should still be detected and the connection should exit.
#[tokio::test(flavor = "current_thread")]
async fn connect_with_on_receive_dispatch_returns_on_transport_close() {
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local
        .run_until(async {
            let (server_reader, server_writer, client_reader, client_writer) = setup_test_streams();

            let server_transport = ByteStreams::new(server_writer, server_reader);
            let client_transport = ByteStreams::new(client_writer, client_reader);

            // Server immediately exits.
            tokio::task::spawn_local(async move {
                UntypedRole
                    .builder()
                    .name("server")
                    .connect_with(server_transport, async move |_cx| Ok(()))
                    .await
                    .ok();
            });

            // Client with an on_receive_dispatch handler and a blocking main_fn.
            let (_outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<Dispatch>();
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                UntypedRole
                    .builder()
                    .name("client")
                    .on_receive_dispatch(
                        async move |_dispatch: Dispatch,
                                    _cx: agent_client_protocol::ConnectionTo<UntypedRole>| {
                            Ok(Handled::Yes)
                        },
                        agent_client_protocol::on_receive_dispatch!(),
                    )
                    .connect_with(client_transport, async move |cx| {
                        while let Some(dispatch) = outgoing_rx.recv().await {
                            cx.send_proxied_message(dispatch)?;
                        }
                        Ok(())
                    }),
            )
            .await;

            assert!(
                result.is_ok(),
                "connect_with with handler should return on transport close, not time out"
            );
            assert!(result.unwrap().is_err());
        })
        .await;
}
