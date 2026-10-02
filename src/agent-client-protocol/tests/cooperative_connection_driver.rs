//! Public-API regressions for cooperative normalized-adapter completion.

use std::{
    future::{self, Future},
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use agent_client_protocol::{
    Channel, ConnectTo, ConnectionDriver, Error, JsonRpcMessage, JsonRpcNotification,
    RawJsonRpcMessage, TransportFrame, UntypedMessage, role::UntypedRole,
};
use futures::{
    FutureExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
    future::Either,
    task::LocalSpawnExt as _,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

const TIMEOUT: Duration = Duration::from_secs(2);
const OUTPUT_COUNT: usize = 3;
const FLUSH_ERROR: &str = "custom physical flush failed";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProbeNotification {
    sequence: usize,
}

impl JsonRpcMessage for ProbeNotification {
    fn matches_method(method: &str) -> bool {
        method == "cooperative-driver-probe"
    }

    fn method(&self) -> &'static str {
        "cooperative-driver-probe"
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

fn frame(sequence: usize) -> TransportFrame {
    TransportFrame::Single(
        RawJsonRpcMessage::notification(
            "cooperative-driver-probe".into(),
            serde_json::json!({ "sequence": sequence }),
        )
        .unwrap(),
    )
}

fn wire_bytes(sequence: usize) -> Vec<u8> {
    let mut bytes = frame(sequence).to_json().unwrap().into_bytes();
    bytes.push(b'\n');
    bytes
}

/// A genuine one-byte physical pipe supplies write backpressure. A separate
/// gate proves that accepting every byte is not equivalent to flushing it.
struct GatedWriter {
    inner: tokio::io::DuplexStream,
    write_blocked: Option<oneshot::Sender<()>>,
    flush_entered: Option<oneshot::Sender<()>>,
    flush_release: oneshot::Receiver<()>,
    written: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
    fail_flush: bool,
}

impl AsyncWrite for GatedWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, bytes);
        match &result {
            Poll::Ready(Ok(count)) => {
                self.written.fetch_add(*count, Ordering::SeqCst);
            }
            Poll::Pending => {
                if let Some(entered) = self.write_blocked.take() {
                    let _ = entered.send(());
                }
            }
            Poll::Ready(Err(_)) => {}
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(entered) = self.flush_entered.take() {
            let _ = entered.send(());
        }
        match Pin::new(&mut self.flush_release).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(_)) => panic!("test dropped the flush gate before release"),
            Poll::Ready(Ok(())) if self.fail_flush => {
                Poll::Ready(Err(io::Error::other(FLUSH_ERROR)))
            }
            Poll::Ready(Ok(())) => Pin::new(&mut self.inner).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_shutdown(cx);
        if matches!(result, Poll::Ready(Ok(()))) {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
        }
        result
    }
}

/// No private SDK actors or built-in transport normalization are involved.
struct NormalizedAdapter {
    channel: Channel,
    driver: ConnectionDriver,
}

impl ConnectTo<UntypedRole> for NormalizedAdapter {
    async fn connect_to(self, client: impl ConnectTo<UntypedRole>) -> Result<(), Error> {
        let bridge = Box::pin(ConnectTo::<UntypedRole>::connect_to(self.channel, client));
        // The adapter owns its producers and closes them when its driver ends.
        match futures::future::select(bridge, self.driver).await {
            Either::Left((result, mut driver)) => {
                result?;
                // The bridge has handed off all accepted finite-peer output.
                if driver.request_finish() {
                    driver.await
                } else {
                    // Preserve an error made ready by the bridge's final poll,
                    // but do not await arbitrary opaque work indefinitely.
                    (&mut driver).now_or_never().unwrap_or(Ok(()))
                }
            }
            Either::Right((result, _bridge)) => result,
        }
    }

    fn into_channel_and_future(self) -> (Channel, Option<ConnectionDriver>) {
        (self.channel, Some(self.driver))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbeMode {
    Normalized,
    Decorated,
    Direct,
}

fn observe_completion(driver: ConnectionDriver, completions: Arc<AtomicUsize>) -> ConnectionDriver {
    driver.map_future(move |work| {
        work.inspect(move |_result| {
            completions.fetch_add(1, Ordering::SeqCst);
        })
    })
}

/// An owned finite peer, not a passive channel whose input must reach EOF.
fn finite_peer(output_count: usize, done: Option<oneshot::Sender<()>>) -> NormalizedAdapter {
    let (channel, physical) = Channel::duplex();
    let driver = ConnectionDriver::new(async move {
        for sequence in 0..output_count {
            physical
                .tx
                .unbounded_send(frame(sequence))
                .map_err(Error::into_internal_error)?;
        }
        if let Some(done) = done {
            done.send(()).unwrap();
        }
        // Close every owned producer on completion; escaped adapter producers
        // in the physical-drain probe remain independently alive.
        drop(physical);
        Ok(())
    });
    NormalizedAdapter { channel, driver }
}

fn seal_output(
    output: &mut mpsc::UnboundedReceiver<TransportFrame>,
    sealed: &mut Option<oneshot::Sender<()>>,
) {
    // Closing the receiver rejects escaped producers but retains accepted frames.
    output.close();
    if let Some(sealed) = sealed.take() {
        let _ = sealed.send(());
    }
}

struct Harness {
    adapter: NormalizedAdapter,
    peer_output: tokio::io::DuplexStream,
    peer_input: tokio::io::DuplexStream,
    escaped_output: mpsc::UnboundedSender<TransportFrame>,
    foreground_done: oneshot::Receiver<()>,
    foreground_signal: oneshot::Sender<()>,
    write_blocked: oneshot::Receiver<()>,
    finish_requested: oneshot::Receiver<()>,
    output_sealed: oneshot::Receiver<()>,
    input_read: oneshot::Receiver<()>,
    flush_input_read: oneshot::Receiver<()>,
    flush_entered: oneshot::Receiver<()>,
    flush_release: oneshot::Sender<()>,
    finish_calls: Arc<AtomicUsize>,
    completions: Arc<AtomicUsize>,
    written: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
}

fn harness(mode: ProbeMode, fail_flush: bool) -> Harness {
    let (channel, physical) = Channel::duplex();
    let escaped_output = channel.tx.clone();
    let Channel {
        rx: mut output,
        tx: input,
    } = physical;
    let (sdk_output, peer_output) = tokio::io::duplex(1);
    let (peer_input, sdk_input) = tokio::io::duplex(4096);
    let (foreground_signal, foreground_done) = oneshot::channel();
    let (write_blocked_tx, write_blocked) = oneshot::channel();
    let (flush_entered_tx, flush_entered) = oneshot::channel();
    let (flush_release, flush_release_rx) = oneshot::channel();
    let (finish_tx, finish_rx) = oneshot::channel();
    let (finish_requested_tx, finish_requested) = oneshot::channel();
    let (output_sealed_tx, output_sealed) = oneshot::channel();
    let (input_read_tx, input_read) = oneshot::channel();
    let (flush_input_read_tx, flush_input_read) = oneshot::channel();
    let stop_delivery = Arc::new(AtomicBool::new(false));
    let callback_stop_delivery = stop_delivery.clone();
    let finish_calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = finish_calls.clone();
    let completions = Arc::new(AtomicUsize::new(0));
    let written = Arc::new(AtomicUsize::new(0));
    let shutdowns = Arc::new(AtomicUsize::new(0));
    let mut writer = GatedWriter {
        inner: sdk_output,
        write_blocked: Some(write_blocked_tx),
        flush_entered: Some(flush_entered_tx),
        flush_release: flush_release_rx,
        written: written.clone(),
        shutdowns: shutdowns.clone(),
        fail_flush,
    };
    let outgoing = async move {
        let mut finish = finish_rx.fuse();
        let mut sealed = Some(output_sealed_tx);
        loop {
            let next = futures::select_biased! {
                signal = finish => {
                    // Losing the hook is not a graceful finish request.
                    if signal.is_ok() {
                        seal_output(&mut output, &mut sealed);
                    }
                    continue;
                },
                next = output.next().fuse() => next,
            };
            let Some(frame) = next else { break };
            let mut bytes = frame.to_json()?.into_bytes();
            bytes.push(b'\n');
            let write = writer.write_all(&bytes).fuse();
            futures::pin_mut!(write);
            futures::select_biased! {
                signal = finish => {
                    if signal.is_ok() {
                        seal_output(&mut output, &mut sealed);
                    }
                    // Never cancel a partially completed physical write.
                    write.await.map_err(Error::into_internal_error)?;
                },
                result = write => result.map_err(Error::into_internal_error)?,
            }
        }
        writer.flush().await.map_err(Error::into_internal_error)?;
        writer
            .shutdown()
            .await
            .map_err(Error::into_internal_error)?;
        Ok(())
    };
    let incoming = async move {
        let mut lines = tokio::io::BufReader::new(sdk_input).lines();
        let mut read = Some(input_read_tx);
        let mut flush_read = Some(flush_input_read_tx);
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(Error::into_internal_error)?
        {
            // The direct bridge drops its receiver when the finite peer ends.
            // Continue reading during physical drain (including genuine read
            // errors), but stop delivering successful input after finish.
            // Normalized modes deliberately still forward late input to test
            // the generic Builder's retain-and-discard behavior.
            if !stop_delivery.load(Ordering::SeqCst) {
                input
                    .unbounded_send(TransportFrame::parse_json(&line))
                    .map_err(Error::into_internal_error)?;
            }
            if let Some(read) = read.take() {
                let _ = read.send(());
            } else if let Some(read) = flush_read.take() {
                let _ = read.send(());
            }
        }
        Ok::<_, Error>(())
    };
    let driver = ConnectionDriver::with_finish(
        async move {
            futures::pin_mut!(outgoing, incoming);
            match futures::future::select(outgoing, incoming).await {
                Either::Left((result, _incoming)) => result,
                Either::Right((result, outgoing)) => {
                    result?;
                    outgoing.await
                }
            }
        },
        move || {
            callback_calls.fetch_add(1, Ordering::SeqCst);
            if mode == ProbeMode::Direct {
                callback_stop_delivery.store(true, Ordering::SeqCst);
            }
            let _ = finish_tx.send(());
            let _ = finish_requested_tx.send(());
        },
    );
    let driver = if mode == ProbeMode::Decorated {
        observe_completion(driver, completions.clone())
    } else {
        driver
    };
    Harness {
        adapter: NormalizedAdapter { channel, driver },
        peer_output,
        peer_input,
        escaped_output,
        foreground_done,
        foreground_signal,
        write_blocked,
        finish_requested,
        output_sealed,
        input_read,
        flush_input_read,
        flush_entered,
        flush_release,
        finish_calls,
        completions,
        written,
        shutdowns,
    }
}

async fn observe<T: std::fmt::Debug>(
    connection: Pin<&mut (impl Future<Output = Result<T, Error>> + ?Sized)>,
    probe: oneshot::Receiver<()>,
    description: &str,
) {
    match tokio::time::timeout(TIMEOUT, futures::future::select(connection, probe))
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {description}"))
    {
        Either::Left((result, _)) => {
            panic!("connection completed before {description}: {result:?}");
        }
        Either::Right((signal, _)) => signal.expect("probe sender dropped"),
    }
}

async fn cooperative_drain_probe(mode: ProbeMode, fail_flush: bool) {
    let Harness {
        adapter,
        mut peer_output,
        mut peer_input,
        escaped_output,
        foreground_done,
        foreground_signal,
        write_blocked,
        finish_requested,
        output_sealed,
        input_read,
        flush_input_read,
        flush_entered,
        flush_release,
        finish_calls,
        completions,
        written,
        shutdowns,
    } = harness(mode, fail_flush);
    let delivered = Arc::new(AtomicUsize::new(0));
    let handler_delivered = delivered.clone();
    let mut connection = if mode == ProbeMode::Direct {
        adapter
            .connect_to(finite_peer(OUTPUT_COUNT, Some(foreground_signal)))
            .map(|result| result.map(|()| 37))
            .boxed()
    } else {
        UntypedRole
            .builder()
            .on_receive_notification(
                async move |_notification: ProbeNotification, _cx| {
                    handler_delivered.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(adapter, async move |cx| {
                for sequence in 0..OUTPUT_COUNT {
                    cx.send_notification(ProbeNotification { sequence })?;
                }
                foreground_signal.send(()).unwrap();
                Ok(37)
            })
            .boxed()
    };
    observe(
        connection.as_mut(),
        foreground_done,
        "finite foreground success",
    )
    .await;
    observe(
        connection.as_mut(),
        write_blocked,
        "physical write backpressure",
    )
    .await;
    observe(
        connection.as_mut(),
        finish_requested,
        "cooperative finish request",
    )
    .await;
    observe(connection.as_mut(), output_sealed, "adapter output sealing").await;
    assert_eq!(finish_calls.load(Ordering::SeqCst), 1);
    assert_eq!(completions.load(Ordering::SeqCst), 0);
    assert_eq!(written.load(Ordering::SeqCst), 1);
    assert_eq!(shutdowns.load(Ordering::SeqCst), 0);
    assert!(escaped_output.unbounded_send(frame(99)).is_err());
    assert!(connection.as_mut().now_or_never().is_none());

    // Input arrives only after foreground success and finish. Normalized modes
    // forward it without restarting application dispatch; Direct discards it
    // without cancelling the still-pending physical write/flush.
    peer_input.write_all(&wire_bytes(99)).await.unwrap();
    observe(
        connection.as_mut(),
        input_read,
        "late physical input during drain",
    )
    .await;
    assert_eq!(delivered.load(Ordering::SeqCst), 0);

    let expected: Vec<_> = (0..OUTPUT_COUNT).flat_map(wire_bytes).collect();
    let mut received = vec![0; expected.len()];
    {
        let read = peer_output.read_exact(&mut received);
        futures::pin_mut!(read);
        match tokio::time::timeout(TIMEOUT, futures::future::select(connection.as_mut(), read))
            .await
            .expect("physical output failed to drain after peer began reading")
        {
            Either::Left((result, _)) => panic!("connection bypassed flush gate: {result:?}"),
            Either::Right((result, _)) => {
                result.unwrap();
            }
        }
    }
    assert_eq!(
        received, expected,
        "all accepted notifications must reach the pipe"
    );
    observe(connection.as_mut(), flush_entered, "physical flush gate").await;
    assert_eq!(written.load(Ordering::SeqCst), expected.len());
    assert!(connection.as_mut().now_or_never().is_none());
    assert_eq!(finish_calls.load(Ordering::SeqCst), 1);
    assert_eq!(completions.load(Ordering::SeqCst), 0);
    assert_eq!(shutdowns.load(Ordering::SeqCst), 0);

    // A second late line forces the input loop to run while flush is blocked,
    // not merely while the one-byte write pipe is backpressured.
    peer_input.write_all(&wire_bytes(100)).await.unwrap();
    observe(
        connection.as_mut(),
        flush_input_read,
        "late physical input at the flush gate",
    )
    .await;
    assert!(connection.as_mut().now_or_never().is_none());
    assert_eq!(completions.load(Ordering::SeqCst), 0);
    flush_release.send(()).unwrap();
    let result = tokio::time::timeout(TIMEOUT, connection.as_mut())
        .await
        .expect("cooperative completion waited for independently open input EOF");
    if fail_flush {
        let error = result.expect_err("foreground success masked the physical flush error");
        assert_eq!(
            error,
            Error::into_internal_error(io::Error::other(FLUSH_ERROR))
        );
        assert_eq!(shutdowns.load(Ordering::SeqCst), 0);
    } else {
        assert_eq!(result.unwrap(), 37);
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
        let mut extra = [0];
        assert_eq!(
            tokio::time::timeout(TIMEOUT, peer_output.read(&mut extra))
                .await
                .expect("adapter failed to half-close physical output")
                .unwrap(),
            0
        );
    }
    assert_eq!(finish_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        completions.load(Ordering::SeqCst),
        usize::from(mode == ProbeMode::Decorated),
        "completion observation must run exactly once, on success or error"
    );
    assert_eq!(delivered.load(Ordering::SeqCst), 0);
    // These owners survive every assertion: neither input EOF nor dropping the
    // escaped output producer is allowed to be the completion trigger.
    drop((peer_input, escaped_output));
}

#[tokio::test]
async fn finite_builder_waits_for_custom_cooperative_drain_not_input_eof() {
    cooperative_drain_probe(ProbeMode::Normalized, false).await;
}

#[tokio::test]
async fn custom_cooperative_flush_error_overrides_foreground_success() {
    cooperative_drain_probe(ProbeMode::Normalized, true).await;
}

#[tokio::test]
async fn decorated_cooperative_driver_waits_for_physical_drain() {
    cooperative_drain_probe(ProbeMode::Decorated, false).await;
}

#[tokio::test]
async fn decorated_cooperative_driver_preserves_physical_flush_error() {
    cooperative_drain_probe(ProbeMode::Decorated, true).await;
}

#[tokio::test]
async fn direct_finite_peer_waits_for_physical_drain_not_input_eof() {
    cooperative_drain_probe(ProbeMode::Direct, false).await;
}

#[tokio::test]
async fn direct_finite_peer_preserves_physical_flush_error() {
    cooperative_drain_probe(ProbeMode::Direct, true).await;
}

#[test]
fn already_requested_finish_survives_driver_handoff() {
    already_requested_finish_probe(false);
}

#[test]
fn already_requested_finish_survives_decoration_and_driver_handoff() {
    already_requested_finish_probe(true);
}

fn already_requested_finish_probe(decorate: bool) {
    let (channel, physical) = Channel::duplex();
    let (finish_tx, finish_rx) = oneshot::channel();
    let (flush_started_tx, mut flush_started_rx) = oneshot::channel();
    let (flush_release_tx, flush_release_rx) = oneshot::channel();
    let finish_calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = finish_calls.clone();
    let completions = Arc::new(AtomicUsize::new(0));
    let flushed = Arc::new(AtomicBool::new(false));
    let driver_flushed = flushed.clone();
    let mut driver = ConnectionDriver::with_finish(
        async move {
            finish_rx.await.unwrap();
            flush_started_tx.send(()).unwrap();
            flush_release_rx.await.unwrap();
            driver_flushed.store(true, Ordering::SeqCst);
            drop(physical);
            Ok(())
        },
        move || {
            callback_calls.fetch_add(1, Ordering::SeqCst);
            finish_tx.send(()).unwrap();
        },
    );
    assert!(driver.request_finish());
    let mut driver = if decorate {
        observe_completion(driver, completions.clone())
    } else {
        driver
    };
    if decorate {
        assert!(driver.request_finish());
    }
    assert_eq!(finish_calls.load(Ordering::SeqCst), 1);
    assert_eq!(completions.load(Ordering::SeqCst), 0);

    let (result_tx, mut result_rx) = oneshot::channel();
    let mut pool = futures::executor::LocalPool::new();
    pool.spawner()
        .spawn_local(async move {
            let result = UntypedRole
                .builder()
                .connect_with(NormalizedAdapter { channel, driver }, async |_cx| Ok(17))
                .await;
            result_tx.send(result).unwrap();
        })
        .unwrap();
    // Drive every ready actor, not just the first Pending poll before the
    // outgoing drain marker has been processed.
    pool.run_until_stalled();
    assert!(
        result_rx.try_recv().unwrap().is_none(),
        "handoff must not downgrade already-finishing work to opaque cancellation"
    );
    assert_eq!(flush_started_rx.try_recv().unwrap(), Some(()));
    assert!(!flushed.load(Ordering::SeqCst));
    assert_eq!(finish_calls.load(Ordering::SeqCst), 1);
    assert_eq!(completions.load(Ordering::SeqCst), 0);

    flush_release_tx.send(()).unwrap();
    pool.run_until_stalled();
    assert_eq!(result_rx.try_recv().unwrap(), Some(Ok(17)));
    assert!(flushed.load(Ordering::SeqCst));
    assert_eq!(finish_calls.load(Ordering::SeqCst), 1);
    assert_eq!(completions.load(Ordering::SeqCst), usize::from(decorate));
}

#[test]
fn finish_request_is_synchronous_and_one_shot_not_future_completion() {
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = calls.clone();
    let (signal, mut received) = oneshot::channel();
    let mut driver =
        ConnectionDriver::with_finish(future::pending::<Result<(), Error>>(), move || {
            callback_calls.fetch_add(1, Ordering::SeqCst);
            signal.send(()).unwrap();
        });
    assert!(driver.request_finish());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!((&mut received).now_or_never(), Some(Ok(())));
    assert!(driver.request_finish());
    assert!(Pin::new(&mut driver).now_or_never().is_none());
    drop(driver);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn dropping_unpolled_cooperative_driver_does_not_request_finish() {
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = calls.clone();
    let (signal, received) = oneshot::channel();
    let driver = ConnectionDriver::with_finish(future::pending::<Result<(), Error>>(), move || {
        callback_calls.fetch_add(1, Ordering::SeqCst);
        let _ = signal.send(());
    });
    drop(driver);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(matches!(received.now_or_never(), Some(Err(_))));
}

struct DropProbe(Arc<AtomicBool>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn finite_foreground_cancels_opaque_pending_driver_instead_of_waiting() {
    opaque_drain_probe(false).await;
}

#[tokio::test]
async fn decorated_opaque_driver_stays_opaque_and_is_cancelled_without_completion() {
    opaque_drain_probe(true).await;
}

async fn opaque_drain_probe(decorate: bool) {
    let (channel, mut peer) = Channel::duplex();
    let dropped = Arc::new(AtomicBool::new(false));
    let completions = Arc::new(AtomicUsize::new(0));
    let guard = DropProbe(dropped.clone());
    let mut driver = ConnectionDriver::new(async move {
        future::pending::<()>().await;
        drop(guard);
        Ok(())
    });
    assert!(
        !driver.request_finish(),
        "opaque work cannot be gracefully finished"
    );
    let mut driver = if decorate {
        observe_completion(driver, completions.clone())
    } else {
        driver
    };
    assert!(!driver.request_finish());
    let mut connection = Box::pin(UntypedRole.builder().connect_with(
        NormalizedAdapter { channel, driver },
        async |cx| {
            cx.send_notification(ProbeNotification { sequence: 0 })?;
            Ok(37)
        },
    ));
    assert_eq!(
        tokio::time::timeout(TIMEOUT, connection.as_mut())
            .await
            .expect("finite foreground waited forever for opaque work")
            .unwrap(),
        37
    );
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(completions.load(Ordering::SeqCst), 0);
    assert_eq!(
        peer.rx
            .next()
            .now_or_never()
            .flatten()
            .unwrap()
            .to_json()
            .unwrap(),
        frame(0).to_json().unwrap()
    );
    drop(peer);
}

fn assert_direct_completion(
    adapter: NormalizedAdapter,
    peer: NormalizedAdapter,
    expected: Result<(), Error>,
) {
    let (result_tx, mut result_rx) = oneshot::channel();
    let mut pool = futures::executor::LocalPool::new();
    pool.spawner()
        .spawn_local(async move {
            result_tx.send(adapter.connect_to(peer).await).unwrap();
        })
        .unwrap();
    pool.run_until_stalled();
    assert_eq!(
        result_rx.try_recv().unwrap(),
        Some(expected),
        "direct finite-peer completion must not await opaque pending work"
    );
}

#[test]
fn direct_finite_peer_cancels_decorated_opaque_pending_driver() {
    let (channel, physical) = Channel::duplex();
    let dropped = Arc::new(AtomicBool::new(false));
    let completions = Arc::new(AtomicUsize::new(0));
    let guard = DropProbe(dropped.clone());
    let driver = ConnectionDriver::new(async move {
        future::pending::<()>().await;
        drop((physical, guard));
        Ok(())
    });
    let mut driver = observe_completion(driver, completions.clone());
    assert!(!driver.request_finish());
    assert_direct_completion(
        NormalizedAdapter { channel, driver },
        finite_peer(0, None),
        Ok(()),
    );
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(completions.load(Ordering::SeqCst), 0);
}

#[test]
fn direct_finite_peer_does_not_mask_a_ready_cooperative_driver_error() {
    let (channel, physical) = Channel::duplex();
    let finish_calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = finish_calls.clone();
    let error = Error::into_internal_error(io::Error::other("ready physical driver failed"));
    let driver_error = error.clone();
    let driver = ConnectionDriver::with_finish(
        async move {
            drop(physical);
            Err(driver_error)
        },
        move || {
            callback_calls.fetch_add(1, Ordering::SeqCst);
        },
    );
    assert_direct_completion(
        NormalizedAdapter { channel, driver },
        finite_peer(0, None),
        Err(error),
    );
    assert_eq!(finish_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn direct_finite_peer_does_not_mask_an_opaque_error_readied_by_bridge_completion() {
    let (channel, physical) = Channel::duplex();
    let (peer_done_tx, peer_done_rx) = oneshot::channel();
    let error = Error::into_internal_error(io::Error::other("final bridge poll readied error"));
    let driver_error = error.clone();
    let driver = ConnectionDriver::new(async move {
        peer_done_rx.await.unwrap();
        drop(physical);
        Err(driver_error)
    });
    assert_direct_completion(
        NormalizedAdapter { channel, driver },
        finite_peer(0, Some(peer_done_tx)),
        Err(error),
    );
}

#[test]
fn direct_owned_driver_completion_does_not_wait_for_independent_peer() {
    let (channel, physical) = Channel::duplex();
    let finish_calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = finish_calls.clone();
    let driver = ConnectionDriver::with_finish(
        async move {
            // A real adapter must flush its accepted physical output before
            // this point. This empty adapter owns/closes both producers.
            drop(physical);
            Ok(())
        },
        move || {
            callback_calls.fetch_add(1, Ordering::SeqCst);
        },
    );
    let (peer_channel, peer_physical) = Channel::duplex();
    let peer_dropped = Arc::new(AtomicBool::new(false));
    let guard = DropProbe(peer_dropped.clone());
    let peer_driver = ConnectionDriver::new(async move {
        future::pending::<()>().await;
        drop((peer_physical, guard));
        Ok(())
    });
    assert_direct_completion(
        NormalizedAdapter { channel, driver },
        NormalizedAdapter {
            channel: peer_channel,
            driver: peer_driver,
        },
        Ok(()),
    );
    assert!(peer_dropped.load(Ordering::SeqCst));
    assert_eq!(finish_calls.load(Ordering::SeqCst), 0);
}
