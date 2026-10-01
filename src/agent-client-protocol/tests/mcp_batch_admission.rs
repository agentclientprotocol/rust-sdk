#![cfg(feature = "unstable_mcp_over_acp")]

use std::{
    future::pending,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agent_client_protocol::{
    Agent, Channel, Client, ConnectionLimits, ConnectionTo, Error, JsonRpcMessage,
    RawJsonRpcMessage, RunWithConnectionTo, TransportBatch, TransportFrame,
    mcp_server::{McpOutcome, McpRequest, McpRequestContext, McpServer, McpService},
    schema::v1,
};
use futures::{StreamExt as _, future::BoxFuture};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

const TIMEOUT: Duration = Duration::from_secs(10);
const BATCHES: usize = 6;
const QUEUE_CAPACITY: usize = 8;

struct NullRun;

impl RunWithConnectionTo<Agent> for NullRun {
    async fn run_with_connection_to(self, _connection: ConnectionTo<Agent>) -> Result<(), Error> {
        pending().await
    }
}

struct CountDrop(Arc<AtomicUsize>);

impl Drop for CountDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct GatedService {
    releases: Mutex<Vec<Option<oneshot::Receiver<()>>>>,
    started: mpsc::Sender<usize>,
    dropped: Arc<AtomicUsize>,
}

fn payload() -> Value {
    json!({"payload": "x".repeat(550)})
}

impl McpService<Agent> for GatedService {
    fn execute(
        &self,
        request: McpRequest,
        context: McpRequestContext<Agent>,
    ) -> BoxFuture<'static, Result<McpOutcome, Error>> {
        let index = usize::try_from(request.params.unwrap()["index"].as_u64().unwrap()).unwrap();
        let release = self.releases.lock().unwrap()[index].take().unwrap();
        let started = self.started.clone();
        let dropped = self.dropped.clone();
        Box::pin(async move {
            let _drop = CountDrop(dropped);
            started
                .send(index)
                .await
                .map_err(Error::into_internal_error)?;
            tokio::select! {
                result = release => result.map_err(Error::into_internal_error)?,
                () = context.operation_cancellation().cancelled() => {
                    return Err(Error::request_cancelled());
                }
            }
            Ok(McpOutcome::Result(payload()))
        })
    }
}

struct BurstService;

impl McpService<Agent> for BurstService {
    fn execute(
        &self,
        _request: McpRequest,
        context: McpRequestContext<Agent>,
    ) -> BoxFuture<'static, Result<McpOutcome, Error>> {
        Box::pin(async move {
            // In one operation poll, fill the entire application queue. The
            // terminal response must wait for a slot, including in a batch.
            for _ in 0..QUEUE_CAPACITY {
                context
                    .send_notification(
                        "notifications/progress",
                        Some(
                            json!({"progressToken": 1, "progress": 1})
                                .as_object()
                                .unwrap()
                                .clone(),
                        ),
                    )
                    .await?;
            }
            Ok(McpOutcome::Result(json!({"admitted": true})))
        })
    }
}

async fn start_provider(
    service: impl McpService<Agent>,
    limits: ConnectionLimits,
) -> (
    Channel,
    v1::McpServerAcpId,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<Result<(), Error>>,
) {
    let (provider, mut peer) = Channel::duplex_with_limits(limits);
    let (ready_tx, ready_rx) = oneshot::channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(
        Client
            .builder()
            .connect_with(provider, async move |connection| {
                connection
                    .build_session_cwd()?
                    .with_mcp_server(McpServer::new_service(service, "batch-admission", NullRun))?
                    .block_task()
                    .run_until(async |_session| {
                        ready_tx.send(()).unwrap();
                        let _ = stop_rx.await;
                        Ok(())
                    })
                    .await
            }),
    );
    let frame = peer.rx.next().await.unwrap().into_frame();
    let TransportFrame::Single(RawJsonRpcMessage::Request(request)) = frame else {
        panic!("expected session/new");
    };
    let request_params = v1::NewSessionRequest::parse_message(&request.method, &request.params)
        .expect("valid session/new request");
    let [v1::McpServer::Acp(server)] = request_params.mcp_servers.as_slice() else {
        panic!("expected native MCP declaration");
    };
    let server_id = server.server_id.clone();
    peer.tx
        .send_frame(TransportFrame::Single(RawJsonRpcMessage::response(
            request.id,
            Ok(serde_json::to_value(v1::NewSessionResponse::new("batch-session")).unwrap()),
        )))
        .await
        .unwrap();
    ready_rx.await.unwrap();
    (peer, server_id, stop_tx, task)
}

fn request(server_id: &v1::McpServerAcpId, index: usize) -> RawJsonRpcMessage {
    let request = v1::MessageMcpRequest::new(
        server_id.clone(),
        format!("logical-{index}"),
        "admission/probe",
    )
    .params(
        json!({
            "index": index,
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
                "progressToken": 1
            }
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    RawJsonRpcMessage::request(
        request.method().to_owned(),
        serde_json::to_value(request).unwrap(),
        format!("wire-{index}").into(),
    )
    .unwrap()
}

fn batch(messages: impl IntoIterator<Item = RawJsonRpcMessage>) -> TransportFrame {
    TransportFrame::Batch(TransportBatch::from_messages(messages).unwrap())
}

fn control_frame_with_bytes(bytes: usize) -> TransportFrame {
    let frame =
        |padding| TransportFrame::Single(RawJsonRpcMessage::response(0.into(), Ok(json!(padding))));
    let envelope_bytes = frame("").to_json().unwrap().len();
    let frame = frame(&"x".repeat(bytes - envelope_bytes));
    assert_eq!(frame.to_json().unwrap().len(), bytes);
    frame
}

#[tokio::test]
async fn partial_native_mcp_batches_fail_byte_admission_instead_of_deadlocking() {
    tokio::time::timeout(TIMEOUT, async {
        let limits = ConnectionLimits {
            max_frame_bytes: 2048,
            max_queued_bytes: 4096,
            max_queued_frames: 32,
        };
        let (started_tx, mut started_rx) = mpsc::channel(2 * BATCHES);
        let (release_tx, release_rx): (Vec<_>, Vec<_>) =
            (0..2 * BATCHES).map(|_| oneshot::channel()).unzip();
        let mut release_tx = release_tx.into_iter().map(Some).collect::<Vec<_>>();
        let dropped = Arc::new(AtomicUsize::new(0));
        let (mut peer, server_id, _stop, mut provider) = start_provider(
            GatedService {
                releases: Mutex::new(release_rx.into_iter().map(Some).collect()),
                started: started_tx,
                dropped: dropped.clone(),
            },
            limits,
        )
        .await;

        // Every complete batch is independently within the frame limit.
        let expected_response = |index: usize| {
            RawJsonRpcMessage::response(
                format!("wire-{index}").into(),
                Ok(serde_json::to_value(v1::MessageMcpResponse::success(payload())).unwrap()),
            )
        };
        let complete = batch([expected_response(0), expected_response(1)]);
        assert!(complete.to_json().unwrap().len() < limits.max_frame_bytes);
        let probe = TransportFrame::Single(expected_response(0));
        let admission = peer.tx.admission();

        // Start all operations before releasing any outcome. Await each pair
        // so inbound data cannot itself exhaust the small shared byte budget.
        for index in 0..BATCHES {
            peer.tx
                .send_frame(batch([
                    request(&server_id, 2 * index),
                    request(&server_id, 2 * index + 1),
                ]))
                .await
                .unwrap();
            let mut started = [
                started_rx.recv().await.unwrap(),
                started_rx.recv().await.unwrap(),
            ];
            started.sort_unstable();
            assert_eq!(started, [2 * index, 2 * index + 1]);
        }

        // Actively drain throughout the regression. No complete response is
        // possible until we release second siblings below.
        let received = Arc::new(AtomicUsize::new(0));
        let received_in_drain = received.clone();
        let drain = tokio::spawn(async move {
            while let Some(frame) = peer.rx.next().await {
                drop(frame);
                received_in_drain.fetch_add(1, Ordering::SeqCst);
            }
        });
        for index in 0..BATCHES {
            release_tx[2 * index].take().unwrap().send(()).unwrap();
        }
        // Observe retained byte pressure, not elapsed time or scheduler turns.
        // All six first replies fit, but leave less than one more reply's
        // worth of capacity. Probing releases its permit immediately.
        loop {
            if admission.try_admit(probe.clone()).is_err() {
                break;
            }
            assert!(!provider.is_finished(), "first siblings must all fit");
            tokio::task::yield_now().await;
        }
        assert_eq!(dropped.load(Ordering::SeqCst), BATCHES);
        assert_eq!(received.load(Ordering::SeqCst), 0);
        assert!(!provider.is_finished(), "partial batches are still live");

        // Only these incomplete batches can release the retained bytes. The
        // old await-byte behavior hangs here despite the draining peer.
        for release in release_tx.into_iter().flatten() {
            release.send(()).unwrap();
        }
        let error = (&mut provider)
            .await
            .unwrap()
            .expect_err("live terminal admission failure must be supervised");
        assert!(
            error
                .to_string()
                .contains("outgoing application byte capacity exceeded"),
            "{error}"
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 2 * BATCHES);
        assert_eq!(received.load(Ordering::SeqCst), 0);
        drain.await.unwrap();
        // Re-admit the entire budget at once, proving teardown released every
        // partial-batch permit rather than merely enough for another response.
        let max_frame = control_frame_with_bytes(limits.max_frame_bytes);
        let _first = admission.try_admit(max_frame.clone()).unwrap();
        let _second = admission.try_admit(max_frame).unwrap();
    })
    .await
    .expect("partial batches hung with a draining peer");
}

#[tokio::test]
async fn ordinary_native_mcp_replies_await_drainable_byte_capacity() {
    tokio::time::timeout(TIMEOUT, async {
        let (started_tx, mut started_rx) = mpsc::channel(BATCHES + 1);
        let (release_tx, release_rx): (Vec<_>, Vec<_>) =
            (0..=BATCHES).map(|_| oneshot::channel()).unzip();
        let dropped = Arc::new(AtomicUsize::new(0));
        let (mut peer, server_id, stop, provider) = start_provider(
            GatedService {
                releases: Mutex::new(release_rx.into_iter().map(Some).collect()),
                started: started_tx,
                dropped: dropped.clone(),
            },
            ConnectionLimits {
                max_frame_bytes: 2048,
                max_queued_bytes: 4096,
                max_queued_frames: 32,
            },
        )
        .await;
        for index in 0..=BATCHES {
            peer.tx
                .send_frame(TransportFrame::Single(request(&server_id, index)))
                .await
                .unwrap();
            assert_eq!(started_rx.recv().await.unwrap(), index);
        }

        let mut releases = release_tx.into_iter();
        let mut retained = Vec::new();
        for _ in 0..BATCHES {
            releases.next().unwrap().send(()).unwrap();
            // These are complete, drainable frames, unlike partial batches.
            retained.push(peer.rx.next().await.unwrap());
        }
        let probe = TransportFrame::Single(RawJsonRpcMessage::response(
            "wire-6".to_owned().into(),
            Ok(serde_json::to_value(v1::MessageMcpResponse::success(payload())).unwrap()),
        ));
        assert!(peer.tx.admission().try_admit(probe).is_err());
        releases.next().unwrap().send(()).unwrap();
        while dropped.load(Ordering::SeqCst) != BATCHES + 1 {
            tokio::task::yield_now().await;
        }
        assert!(!provider.is_finished(), "ordinary reply must await bytes");
        drop(retained);
        let reply = peer.rx.next().await.unwrap().into_frame();
        let reply: Value = serde_json::from_str(&reply.to_json().unwrap()).unwrap();
        assert_eq!(reply["id"], "wire-6");
        assert_eq!(reply["result"]["result"], payload());

        stop.send(()).unwrap();
        assert!(peer.rx.next().await.is_none());
        provider
            .await
            .unwrap()
            .expect("ordinary byte backpressure must recover");
    })
    .await
    .expect("ordinary reply did not recover when complete frames drained");
}

#[tokio::test]
async fn native_mcp_batch_notification_burst_awaits_queue_slots() {
    tokio::time::timeout(TIMEOUT, async {
        let (mut peer, server_id, stop, provider) = start_provider(
            BurstService,
            ConnectionLimits {
                max_queued_frames: QUEUE_CAPACITY,
                ..ConnectionLimits::default()
            },
        )
        .await;
        // One entry still has a batch destination, but avoids a second service
        // competing for slots and makes the full-queue boundary deterministic.
        for _ in 0..2 {
            // Reuse the logical ID only after its terminal response arrives.
            peer.tx
                .send_frame(batch([request(&server_id, 0)]))
                .await
                .unwrap();
            for _ in 0..QUEUE_CAPACITY {
                let frame = peer.rx.next().await.unwrap().into_frame();
                let TransportFrame::Single(RawJsonRpcMessage::Notification(notification)) = frame
                else {
                    panic!("terminal batch overtook a notification");
                };
                let notification = v1::MessageMcpNotification::parse_message(
                    &notification.method,
                    &notification.params,
                )
                .unwrap();
                assert_eq!(notification.request_id, v1::McpRequestId::new("logical-0"));
            }
            let frame = peer.rx.next().await.unwrap().into_frame();
            let value: Value = serde_json::from_str(&frame.to_json().unwrap()).unwrap();
            assert_eq!(
                value,
                json!([{"jsonrpc": "2.0", "id": "wire-0", "result": {"result": {"admitted": true}}}])
            );
        }
        stop.send(()).unwrap();
        // Continue draining until the provider's clean shutdown closes output.
        if let Some(frame) = peer.rx.next().await {
            panic!("unexpected output after terminal batch: {frame:?}");
        }
        provider.await.unwrap().expect("clean batch completion");
    })
    .await
    .expect("batch notification burst lost its terminal reply");
}
