//! Public-API probes for owned completion and physical transport normalization.

use std::{
    future, io,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use agent_client_protocol::{
    Agent, ByteStreams, Channel, Client, ConnectTo, ConnectionDriver, Error, JsonRpcMessage,
    JsonRpcNotification, Lines, RawJsonRpcMessage, RawJsonRpcResponse, TransportBatch,
    TransportFrame, UntypedMessage,
    role::{Role, UntypedRole},
    schema::v1::RequestId,
};
use futures::{FutureExt as _, StreamExt as _, future::Either};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProbeNotification {
    sequence: usize,
    payload: String,
}

impl JsonRpcMessage for ProbeNotification {
    fn matches_method(method: &str) -> bool {
        method == "normalization-probe"
    }

    fn method(&self) -> &'static str {
        "normalization-probe"
    }

    fn to_untyped_message(&self) -> Result<UntypedMessage, Error> {
        UntypedMessage::new(self.method(), self)
    }

    fn parse_message(method: &str, params: &impl Serialize) -> Result<Self, Error> {
        if !Self::matches_method(method) {
            return Err(Error::method_not_found());
        }
        agent_client_protocol::util::json_cast(params)
    }
}

impl JsonRpcNotification for ProbeNotification {}

fn notification(sequence: usize) -> ProbeNotification {
    ProbeNotification {
        sequence,
        payload: "x".repeat(1024),
    }
}

fn frame(sequence: usize) -> TransportFrame {
    let notification = notification(sequence);
    TransportFrame::Single(
        RawJsonRpcMessage::notification(
            notification.method().into(),
            serde_json::to_value(notification).unwrap(),
        )
        .unwrap(),
    )
}

fn wire_bytes(sequence: usize) -> Vec<u8> {
    let TransportFrame::Single(message) = frame(sequence) else {
        unreachable!()
    };
    let mut bytes = serde_json::to_vec(&message).unwrap();
    bytes.push(b'\n');
    bytes
}

/// Counts bytes accepted by the actual duplex write half, not by an SDK queue.
/// The duplex has capacity one, so delivery requires concurrent peer reads.
struct PhysicalWriter<W> {
    inner: W,
    written: Arc<AtomicUsize>,
}

impl<W: AsyncWrite + Unpin> AsyncWrite for PhysicalWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, bytes);
        if let Poll::Ready(Ok(count)) = result {
            self.written.fetch_add(count, Ordering::SeqCst);
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[tokio::test]
async fn agent_connect_with_drains_final_notification_to_physical_bytes() {
    let (sdk_outgoing, mut peer_incoming) = tokio::io::duplex(1);
    let (remote_input_guard, sdk_incoming) = tokio::io::duplex(1);
    let written = Arc::new(AtomicUsize::new(0));
    let transport = ByteStreams::new(
        PhysicalWriter {
            inner: sdk_outgoing,
            written: written.clone(),
        }
        .compat_write(),
        sdk_incoming.compat(),
    );
    let expected = wire_bytes(0);
    let mut connection = Box::pin(async {
        Agent
            .builder()
            .connect_with(transport, async |cx| {
                cx.send_notification(notification(0))?;
                Ok(())
            })
            .await
            .expect("finite foreground should complete cleanly");
        assert_eq!(
            written.load(Ordering::SeqCst),
            expected.len(),
            "connect_with returned before accepted notification reached the physical writer"
        );
    });
    let mut peer = Box::pin(async {
        let mut received = vec![0; expected.len()];
        peer_incoming.read_exact(&mut received).await.unwrap();
        assert_eq!(received, expected);
    });
    // Borrow both futures: even a timeout keeps transport work and the peer
    // read half alive until the assertion, alongside the remote input guard.
    tokio::time::timeout(TIMEOUT, async {
        tokio::join!(connection.as_mut(), peer.as_mut());
    })
    .await
    .expect("finite foreground waited for remote EOF instead of draining accepted bytes");
    // Keep physical input open through completion and all assertions.
    drop(remote_input_guard);
}

#[tokio::test]
async fn client_connect_with_drains_final_notification_to_physical_lines() {
    let (sdk_outgoing, mut peer_incoming) = tokio::io::duplex(1);
    let (remote_input_guard, sdk_incoming) = tokio::io::duplex(1);
    let written = Arc::new(AtomicUsize::new(0));
    let writer = PhysicalWriter {
        inner: sdk_outgoing,
        written: written.clone(),
    };
    let outgoing = futures::sink::unfold(writer, async |mut writer, line: String| {
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok::<_, io::Error>(writer)
    });
    let incoming =
        futures::io::AsyncBufReadExt::lines(futures::io::BufReader::new(sdk_incoming.compat()));
    let transport = Lines::new(outgoing, incoming);
    let expected = wire_bytes(0);
    let mut connection = Box::pin(async {
        Client
            .builder()
            .connect_with(transport, async |cx| {
                cx.send_notification(notification(0))?;
                Ok(())
            })
            .await
            .expect("finite foreground should complete cleanly");
        assert_eq!(
            written.load(Ordering::SeqCst),
            expected.len(),
            "connect_with returned before accepted notification reached the physical line sink"
        );
    });
    let mut peer = Box::pin(async {
        let mut received = vec![0; expected.len()];
        peer_incoming.read_exact(&mut received).await.unwrap();
        assert_eq!(received, expected);
    });
    tokio::time::timeout(TIMEOUT, async {
        tokio::join!(connection.as_mut(), peer.as_mut());
    })
    .await
    .expect("finite foreground waited for remote EOF instead of draining accepted lines");
    drop(remote_input_guard);
}

async fn finite_builder_physical_drain_probe(read_error: bool, buffered_input: bool) {
    let (incoming_tx, mut incoming_rx) = futures::channel::mpsc::unbounded();
    let (eof_tx, eof_rx) = futures::channel::oneshot::channel();
    let mut eof_tx = Some(eof_tx);
    let incoming = futures::stream::poll_fn(move |cx| {
        let result = incoming_rx.poll_next_unpin(cx);
        if matches!(result, Poll::Ready(None | Some(Err(_))))
            && let Some(eof_tx) = eof_tx.take()
        {
            eof_tx.send(()).unwrap();
        }
        result
    });
    let (entered_tx, entered_rx) = futures::channel::oneshot::channel();
    let (release_tx, release_rx) = futures::channel::oneshot::channel();
    let delivered = Arc::new(Mutex::new(Vec::new()));
    let outgoing = futures::sink::unfold(
        (Some(entered_tx), Some(release_rx), delivered.clone()),
        async |(mut entered, mut release, delivered), line: String| {
            if let Some(entered) = entered.take() {
                entered.send(()).unwrap();
                release.take().unwrap().await.unwrap();
            }
            delivered.lock().unwrap().push(line);
            Ok::<_, io::Error>((entered, release, delivered))
        },
    );
    let closes = Arc::new(AtomicUsize::new(0));
    let callback_closes = closes.clone();
    let messages = Arc::new(AtomicUsize::new(0));
    let callback_messages = messages.clone();
    let foreground_input = incoming_tx.clone();
    let mut connection = Box::pin(
        UntypedRole
            .builder()
            .on_receive_notification(
                async move |_notification: ProbeNotification, _cx| {
                    callback_messages.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_close(async move |cx| {
                callback_closes.fetch_add(1, Ordering::SeqCst);
                // This is valid while serving, but would fail if a late EOF
                // starts the callback after the outgoing drain marker seals.
                cx.send_notification(notification(1))?;
                Ok(())
            })
            .connect_with(Lines::new(outgoing, incoming), async move |cx| {
                cx.send_notification(notification(0))?;
                if buffered_input {
                    // Input is accepted just as main_fn succeeds, before the
                    // next protocol/physical poll. Stop delivery without
                    // turning a live producer into "receiver is gone".
                    foreground_input
                        .unbounded_send(Ok(frame(3).to_json().unwrap()))
                        .unwrap();
                }
                Ok(())
            }),
    );
    match tokio::time::timeout(
        TIMEOUT,
        futures::future::select(connection.as_mut(), entered_rx),
    )
    .await
    .expect("accepted output never reached the gated physical sink")
    {
        Either::Left((result, _)) => panic!("connection completed before sink release: {result:?}"),
        Either::Right((entered, _)) => entered.unwrap(),
    }
    assert!(connection.as_mut().now_or_never().is_none());
    assert!(delivered.lock().unwrap().is_empty());

    // Peer EOF/error arrives only AFTER foreground success and sink entry.
    // Keep the connection and sink gate alive through every assertion.
    if read_error {
        incoming_tx
            .unbounded_send(Err(io::Error::other("late physical read failed")))
            .unwrap();
    }
    drop(incoming_tx);
    if read_error {
        let error = tokio::time::timeout(TIMEOUT, connection.as_mut())
            .await
            .expect("physical read errors must remain driven during drain")
            .expect_err("physical read errors must not be hidden by foreground success");
        assert!(
            error
                .data
                .unwrap()
                .to_string()
                .contains("late physical read failed")
        );
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        assert_eq!(messages.load(Ordering::SeqCst), 0);
        assert!(delivered.lock().unwrap().is_empty());
        return;
    }
    match tokio::time::timeout(
        TIMEOUT,
        futures::future::select(connection.as_mut(), eof_rx),
    )
    .await
    .expect("physical input stopped progressing during drain")
    {
        Either::Left((result, _)) => panic!("late clean EOF aborted gated drain: {result:?}"),
        Either::Right((eof, _)) => eof.unwrap(),
    }
    let result = connection.as_mut().now_or_never();
    assert!(
        result.is_none(),
        "late clean EOF aborted the gated physical drain: {result:?}"
    );
    assert_eq!(closes.load(Ordering::SeqCst), 0);
    assert!(delivered.lock().unwrap().is_empty());

    release_tx.send(()).unwrap();
    tokio::time::timeout(TIMEOUT, connection.as_mut())
        .await
        .expect("physical drain failed to finish after sink release")
        .expect("clean EOF must not discard accepted output");
    assert_eq!(
        *delivered.lock().unwrap(),
        vec![frame(0).to_json().unwrap()]
    );
    assert_eq!(closes.load(Ordering::SeqCst), 0);
    assert_eq!(messages.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn finite_builder_does_not_start_close_delivery_during_physical_drain() {
    finite_builder_physical_drain_probe(false, false).await;
}

#[tokio::test]
async fn finite_builder_keeps_physical_read_errors_during_drain() {
    finite_builder_physical_drain_probe(true, false).await;
}

#[tokio::test]
async fn finite_builder_stops_buffered_input_delivery_without_closing_raw_receiver_early() {
    finite_builder_physical_drain_probe(false, true).await;
}

#[tokio::test]
async fn underway_close_keeps_callback_phase_task_progress_before_sealing_output() {
    let (channel, peer) = Channel::duplex();
    let Channel { mut rx, tx } = peer;
    drop(tx);
    let (entered_tx, entered_rx) = futures::channel::oneshot::channel();
    let (release_tx, release_rx) = futures::channel::oneshot::channel();
    let task_started = Arc::new(AtomicUsize::new(0));
    let callback_finished = Arc::new(AtomicUsize::new(0));
    let callback_done = callback_finished.clone();
    let started = task_started.clone();
    let mut connection = Box::pin(
        UntypedRole
            .builder()
            .on_close(async move |cx| {
                cx.send_notification(notification(0))?;
                entered_tx.send(()).unwrap();
                release_rx.await.unwrap();
                // This send happens AFTER foreground success. The outgoing
                // boundary must still be open until this callback finishes.
                cx.send_notification(notification(2))?;
                callback_done.store(1, Ordering::SeqCst);
                Ok(())
            })
            .connect_with(channel, async move |cx| {
                entered_rx.await.unwrap();
                let task_cx = cx.clone();
                // Preserve the inherited callback-phase policy: queued tasks
                // can start after main_fn succeeds to unblock close cleanup.
                cx.spawn(async move {
                    started.fetch_add(1, Ordering::SeqCst);
                    task_cx.send_notification(notification(1))?;
                    release_tx.send(()).unwrap();
                    Ok(())
                })?;
                Ok(37)
            }),
    );
    assert_eq!(
        tokio::time::timeout(TIMEOUT, connection.as_mut())
            .await
            .expect("close callback lost application cleanup progress")
            .expect("callback output must be accepted before drain sealing"),
        37
    );
    assert_eq!(task_started.load(Ordering::SeqCst), 1);
    assert_eq!(callback_finished.load(Ordering::SeqCst), 1);
    let mut frames = Vec::new();
    while let Some(frame) = rx.next().now_or_never().flatten() {
        frames.push(frame.to_json().unwrap());
    }
    assert_eq!(
        frames,
        (0..3)
            .map(|sequence| frame(sequence).to_json().unwrap())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn success_does_not_resume_message_delivery_or_start_queued_tasks_for_drain() {
    let (channel, peer) = Channel::duplex();
    let Channel { mut rx, tx } = peer;
    tx.unbounded_send(TransportFrame::Batch(
        TransportBatch::from_messages((0..2).map(|sequence| {
            let TransportFrame::Single(message) = frame(sequence) else {
                unreachable!()
            };
            message
        }))
        .unwrap(),
    ))
    .unwrap();
    let (entered_tx, entered_rx) = futures::channel::oneshot::channel();
    let (release_tx, release_rx) = futures::channel::oneshot::channel();
    let mut entered_tx = Some(entered_tx);
    let mut release_rx = Some(release_rx);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let handler_observed = observed.clone();
    let task_started = Arc::new(AtomicUsize::new(0));
    let started = task_started.clone();
    let mut connection = Box::pin(
        UntypedRole
            .builder()
            .on_receive_notification(
                async move |notification: ProbeNotification, _cx| {
                    handler_observed.lock().unwrap().push(notification.sequence);
                    if notification.sequence == 0 {
                        entered_tx.take().unwrap().send(()).unwrap();
                        release_rx.take().unwrap().await.unwrap();
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(channel, async move |cx| {
                entered_rx.await.unwrap();
                cx.spawn(async move {
                    started.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })?;
                cx.send_notification(notification(2))?;
                // Both this handler and main_fn become ready together. Success
                // must win before the handler can start the next batch entry.
                release_tx.send(()).unwrap();
                Ok(())
            }),
    );
    tokio::time::timeout(TIMEOUT, connection.as_mut())
        .await
        .expect("finite success awaited unrelated application work")
        .expect("accepted output must still drain");
    assert_eq!(*observed.lock().unwrap(), vec![0]);
    assert_eq!(task_started.load(Ordering::SeqCst), 0);
    assert_eq!(
        rx.next()
            .now_or_never()
            .flatten()
            .unwrap()
            .to_json()
            .unwrap(),
        frame(2).to_json().unwrap()
    );
    // The peer input remains open through completion; no close phase justifies
    // starting the queued application task.
    drop(tx);
}

struct DrivenEndpoint {
    channel: Channel,
    driver: Option<ConnectionDriver>,
}

impl<R: Role> ConnectTo<R> for DrivenEndpoint {
    async fn connect_to(self, client: impl ConnectTo<R::Counterpart>) -> Result<(), Error> {
        let bridge = ConnectTo::<R>::connect_to(self.channel, client);
        if let Some(driver) = self.driver {
            futures::try_join!(bridge, driver)?;
        } else {
            bridge.await?;
        }
        Ok(())
    }

    fn into_channel_and_future(self) -> (Channel, Option<ConnectionDriver>) {
        (self.channel, self.driver)
    }
}

async fn reactive_completion_probe(owned: bool) {
    let (channel, peer) = Channel::duplex();
    let Channel {
        rx: remote_receive_guard,
        tx: escaped_producer,
    } = peer;
    for sequence in 0..3 {
        escaped_producer.unbounded_send(frame(sequence)).unwrap();
    }
    // No opaque EOF-dependent future: work is already complete and all accepted
    // frames are in the channel. An escaped producer is not additional owned work.
    let driver = owned.then(|| ConnectionDriver::new(future::ready(Ok(()))));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let seen = observed.clone();
    let (entered_tx, entered_rx) = futures::channel::oneshot::channel();
    let (release_tx, release_rx) = futures::channel::oneshot::channel();
    let mut entered_tx = Some(entered_tx);
    let mut release_rx = Some(release_rx);
    let mut connection = Box::pin(
        UntypedRole
            .builder()
            .on_receive_notification(
                async move |notification: ProbeNotification, _cx| {
                    seen.lock().unwrap().push(notification.sequence);
                    if notification.sequence == 2 {
                        entered_tx.take().unwrap().send(()).unwrap();
                        release_rx.take().unwrap().await.unwrap();
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_to(DrivenEndpoint { channel, driver }),
    );
    // Drive both until the final accepted dispatch is inside its handler.
    let dispatch = tokio::time::timeout(
        TIMEOUT,
        futures::future::select(connection.as_mut(), entered_rx),
    )
    .await
    .expect("accepted notifications never reached the reactive handler");
    match dispatch {
        Either::Left((result, _)) => {
            panic!("connection finished before accepted dispatch completed: {result:?}");
        }
        Either::Right((entered, _)) => entered.unwrap(),
    }
    assert!(
        connection.as_mut().now_or_never().is_none(),
        "connection returned while the final accepted handler was blocked"
    );
    release_tx.send(()).unwrap();

    if owned {
        // Borrow the future through the timeout. On timeout the endpoint, SDK
        // actors, escaped producer, and remote receiver are all still alive.
        let result = tokio::time::timeout(TIMEOUT, connection.as_mut()).await;
        assert!(
            result.is_ok(),
            "owned completion waited for escaped producer EOF: dispatched={:?}, producer_closed={}",
            observed.lock().unwrap(),
            escaped_producer.is_closed()
        );
        result.unwrap().expect("owned completion should be clean");
        assert_eq!(*observed.lock().unwrap(), vec![0, 1, 2]);
        assert!(
            escaped_producer.unbounded_send(frame(3)).is_err(),
            "owned completion still accepts output from an escaped producer"
        );
    } else {
        // None is absence of owned work, not completion. Poll to quiescence,
        // then prove the passive producer can still supply another frame.
        assert!(connection.as_mut().now_or_never().is_none());
        escaped_producer.unbounded_send(frame(3)).unwrap();
        escaped_producer.close_channel();
        tokio::time::timeout(TIMEOUT, connection.as_mut())
            .await
            .expect("passive connection failed to finish after real input EOF")
            .expect("passive EOF should be clean");
        assert_eq!(*observed.lock().unwrap(), vec![0, 1, 2, 3]);
    }
    drop((escaped_producer, remote_receive_guard));
}

#[tokio::test]
async fn reactive_builder_owned_completion_closes_producer_after_accepted_dispatch() {
    reactive_completion_probe(true).await;
}

#[tokio::test]
async fn reactive_builder_without_driver_preserves_passive_producer() {
    reactive_completion_probe(false).await;
}

#[tokio::test]
async fn normalized_split_duplex_half_close_allows_final_reverse_response() {
    // Both SDK halves share ONE underlying stream. Dropping only its write
    // wrapper cannot substitute for AsyncWrite::close while its read half lives.
    let (sdk_stream, peer_stream) = tokio::io::duplex(1);
    let (sdk_incoming, sdk_outgoing) = tokio::io::split(sdk_stream);
    let (peer_incoming, mut peer_outgoing) = tokio::io::split(peer_stream);
    let transport = ByteStreams::new(sdk_outgoing.compat_write(), sdk_incoming.compat());
    let (channel, driver) = ConnectTo::<UntypedRole>::into_channel_and_future(transport);
    let mut driver = Box::pin(driver.expect("ByteStreams must own transport work"));
    let Channel { mut rx, tx } = channel;
    tx.unbounded_send(TransportFrame::Single(
        RawJsonRpcMessage::request(
            "normalization-request".into(),
            serde_json::json!({}),
            RequestId::Number(41),
        )
        .unwrap(),
    ))
    .unwrap();
    drop(tx);

    let stage = Arc::new(AtomicUsize::new(0));
    let peer_stage = stage.clone();
    let mut peer = Box::pin(async move {
        let mut lines = tokio::io::BufReader::new(peer_incoming).lines();
        let request = lines.next_line().await.unwrap().expect("queued request");
        let RawJsonRpcMessage::Request(request) = serde_json::from_str(&request).unwrap() else {
            panic!("peer expected a request");
        };
        assert_eq!(request.id, RequestId::Number(41));
        peer_stage.store(1, Ordering::SeqCst);
        assert!(
            lines.next_line().await.unwrap().is_none(),
            "dropping Channel.tx must produce physical write EOF"
        );
        peer_stage.store(2, Ordering::SeqCst);
        let response =
            RawJsonRpcMessage::response(request.id, Ok(serde_json::json!({ "status": "final" })));
        let mut bytes = serde_json::to_vec(&response).unwrap();
        bytes.push(b'\n');
        peer_outgoing.write_all(&bytes).await.unwrap();
        peer_outgoing.shutdown().await.unwrap();
        peer_stage.store(3, Ordering::SeqCst);
    });
    let mut receive = Box::pin(async {
        let Some(TransportFrame::Single(RawJsonRpcMessage::Response(RawJsonRpcResponse::Result {
            id,
            result,
        }))) = rx.next().await
        else {
            panic!("SDK read half lost the final reverse response");
        };
        assert_eq!(id, RequestId::Number(41));
        assert_eq!(result["status"], "final");
    });
    // Borrow all three futures so timeout does not first destroy either read
    // half (or either peer half) and artificially create physical EOF.
    let result = tokio::time::timeout(TIMEOUT, async {
        let (result, (), ()) = tokio::join!(driver.as_mut(), peer.as_mut(), receive.as_mut());
        result
    })
    .await;
    assert!(
        result.is_ok(),
        "split duplex stalled: peer stage={} (0=request pending, 1=waiting for physical EOF, 2=response writing, 3=response sent); SDK driver/read half and peer halves still retained",
        stage.load(Ordering::SeqCst)
    );
    result
        .unwrap()
        .expect("normalized transport should finish cleanly");
    assert_eq!(stage.load(Ordering::SeqCst), 3);
}

struct CloseFailWriter {
    written: Arc<AtomicUsize>,
}

impl futures::AsyncWrite for CloseFailWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        // Exercise repeated partial writes as well as close-error propagation.
        let count = bytes.len().min(7);
        self.written.fetch_add(count, Ordering::SeqCst);
        Poll::Ready(Ok(count))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::Error::other("physical close failed")))
    }
}

#[tokio::test]
async fn direct_builder_reports_physical_close_error_after_writing_accepted_output() {
    let written = Arc::new(AtomicUsize::new(0));
    let transport = ByteStreams::new(
        CloseFailWriter {
            written: written.clone(),
        },
        futures::io::Cursor::new(Vec::<u8>::new()),
    );
    let error = tokio::time::timeout(
        TIMEOUT,
        Agent.builder().connect_with(transport, async |cx| {
            cx.send_notification(notification(0))?;
            Ok(())
        }),
    )
    .await
    .expect("close failure must complete the connection")
    .expect_err("physical close errors must not be swallowed");
    assert_eq!(written.load(Ordering::SeqCst), wire_bytes(0).len());
    assert!(
        error
            .data
            .unwrap()
            .to_string()
            .contains("physical close failed")
    );
}
