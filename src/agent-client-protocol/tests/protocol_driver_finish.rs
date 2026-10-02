//! Protocol wrappers must preserve owned completion without requiring input EOF.

#![cfg(feature = "unstable_protocol_v2")]

use std::{
    future, io,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use agent_client_protocol::{
    Agent, Channel, Client, Conductor, ConnectTo, ConnectionDriver, Error, JsonRpcNotification,
    Lines, Proxy, RawJsonRpcMessage, RawJsonRpcResponse, Role, TransportFrame,
    schema::{InitializeProxyRequest, ProtocolVersion, v1},
};
use futures::{
    FutureExt as _, SinkExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const DEADLINE: Duration = Duration::from_secs(2);
const FINAL_METHOD: &str = "_test/protocol-driver-final";

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcNotification)]
#[notification(method = "_test/protocol-driver-final")]
struct FinalNotification {
    sequence: usize,
}

struct FiniteClient;

impl ConnectTo<Agent> for FiniteClient {
    async fn connect_to(self, agent: impl ConnectTo<Client>) -> Result<(), Error> {
        Client
            .builder()
            .connect_with(agent, async |cx| {
                let response = cx
                    .send_request(v1::InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert_eq!(response.protocol_version, ProtocolVersion::V1);
                cx.send_notification(FinalNotification { sequence: 1 })?;
                Ok(())
            })
            .await
    }
}

struct FiniteAgent;

impl ConnectTo<Client> for FiniteAgent {
    async fn connect_to(self, client: impl ConnectTo<Agent>) -> Result<(), Error> {
        let (initialized, mut initialization) = mpsc::unbounded();
        Agent
            .builder()
            .on_receive_request(
                async move |request: v1::InitializeRequest, responder, _cx| {
                    assert_eq!(request.protocol_version, ProtocolVersion::V1);
                    responder.respond(v1::InitializeResponse::new(request.protocol_version))?;
                    initialized
                        .unbounded_send(())
                        .map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(client, async move |cx| {
                initialization.next().await.expect("initialize was handled");
                cx.send_notification(FinalNotification { sequence: 1 })?;
                Ok(())
            })
            .await
    }
}

struct FiniteProxy;

impl ConnectTo<Conductor> for FiniteProxy {
    async fn connect_to(self, conductor: impl ConnectTo<Proxy>) -> Result<(), Error> {
        let (initialized, mut initialization) = mpsc::unbounded();
        Proxy
            .builder()
            .on_receive_request_from(
                Client,
                async move |request: InitializeProxyRequest, responder, _cx| {
                    assert_eq!(request.initialize.protocol_version, ProtocolVersion::V1);
                    responder.respond(v1::InitializeResponse::new(
                        request.initialize.protocol_version,
                    ))?;
                    initialized
                        .unbounded_send(())
                        .map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(conductor, async move |cx| {
                initialization.next().await.expect("initialize was handled");
                cx.send_notification_to(Client, FinalNotification { sequence: 1 })?;
                Ok(())
            })
            .await
    }
}

/// Normalization must pass through the original owned driver, including its hook.
struct NormalizedAdapter {
    channel: Channel,
    driver: ConnectionDriver,
}

impl<R: Role> ConnectTo<R> for NormalizedAdapter {
    async fn connect_to(self, peer: impl ConnectTo<R::Counterpart>) -> Result<(), Error> {
        futures::try_join!(ConnectTo::<R>::connect_to(self.channel, peer), self.driver)?;
        Ok(())
    }

    fn into_channel_and_future(self) -> (Channel, Option<ConnectionDriver>) {
        (self.channel, Some(self.driver))
    }
}

struct DropSignal(Arc<AtomicBool>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn frame_value(frame: TransportFrame) -> Result<Value, Error> {
    serde_json::from_str(&frame.to_json()?).map_err(Error::into_internal_error)
}

fn assert_final(value: &Value) {
    assert_eq!(value["jsonrpc"], "2.0");
    assert_eq!(value["method"], FINAL_METHOD);
    assert_eq!(value["params"], json!({ "sequence": 1 }));
    assert!(value.get("id").is_none(), "{value}");
}

fn assert_rejection(value: &Value) {
    assert_eq!(value["jsonrpc"], "2.0");
    assert_eq!(value["id"], 7);
    assert_eq!(value["error"]["code"], -32602);
    assert!(value.get("result").is_none(), "{value}");
}

/// Exercise Lines' actual JSON serialization and sink closure. The incoming
/// sender remains alive until both output EOF and successful router completion.
async fn run_over_lines<R: Role>(
    component: impl ConnectTo<R>,
    method: &str,
    params: Value,
) -> Result<Vec<Value>, Error> {
    let (input_guard, incoming) = mpsc::unbounded::<io::Result<String>>();
    let (outgoing, mut output) = mpsc::unbounded::<String>();
    let lines = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
    input_guard
        .unbounded_send(Ok(json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": method,
            "params": params,
        })
        .to_string()))
        .map_err(Error::into_internal_error)?;

    let mut frames = Vec::new();
    let result = tokio::time::timeout(DEADLINE, async {
        futures::try_join!(component.connect_to(lines), async {
            while let Some(line) = output.next().await {
                frames.push(
                    serde_json::from_str::<Value>(&line).map_err(Error::into_internal_error)?,
                );
            }
            Ok::<_, Error>(())
        })
    })
    .await;
    assert!(
        result.is_ok(),
        "router must finish with remote input open; observed wire frames: {frames:?}"
    );
    result.expect("deadline checked")?;
    // Do not let input EOF make the completion assertion pass.
    drop(input_guard);
    Ok(frames)
}

#[tokio::test(flavor = "current_thread")]
async fn client_connector_cancels_opaque_driver_after_final_frame() -> Result<(), Error> {
    let (channel, Channel { mut rx, tx }) = Channel::duplex();
    let input_guard = tx.clone();
    let dropped = Arc::new(AtomicBool::new(false));
    let drop_signal = DropSignal(dropped.clone());
    let adapter = NormalizedAdapter {
        channel,
        driver: ConnectionDriver::new(async move {
            // Capture the guard before polling, so cancellation is observable
            // even if the opaque future has never started.
            let _drop_signal = drop_signal;
            future::pending::<Result<(), Error>>().await
        }),
    };
    let final_observed = Arc::new(AtomicBool::new(false));
    let peer_observed = final_observed.clone();
    let peer = async move {
        let frame = rx.next().await.expect("connector sends initialize");
        let TransportFrame::Single(RawJsonRpcMessage::Request(request)) = frame else {
            panic!("expected a single initialize request, got {frame:?}");
        };
        assert_eq!(request.method.as_ref(), "initialize");
        tx.unbounded_send(TransportFrame::Single(RawJsonRpcMessage::Response(
            RawJsonRpcResponse::Result {
                id: request.id,
                result: serde_json::to_value(v1::InitializeResponse::new(ProtocolVersion::V1))
                    .map_err(Error::into_internal_error)?,
            },
        )))
        .map_err(Error::into_internal_error)?;
        assert_final(&frame_value(
            rx.next().await.expect("final notification is handed off"),
        )?);
        peer_observed.store(true, Ordering::SeqCst);
        Ok::<_, Error>(())
    };
    let connector = Client.protocol_connector().with_v1(|| FiniteClient);
    let mut adapter = Some(adapter);
    let result = tokio::time::timeout(DEADLINE, async {
        futures::try_join!(
            connector.connect_to(move || adapter.take().expect("v1 transport opened once")),
            peer,
        )
    })
    .await;
    assert!(
        result.is_ok(),
        "connector must cancel opaque owned work after handoff; final frame observed: {}",
        final_observed.load(Ordering::SeqCst)
    );
    result.expect("deadline checked")?;
    assert!(final_observed.load(Ordering::SeqCst));
    assert!(
        dropped.load(Ordering::SeqCst),
        "successful connector completion must drop the pending opaque driver"
    );
    drop(input_guard);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_initialize_flushes_lines_without_input_eof() -> Result<(), Error> {
    let frames = run_over_lines(
        Agent.protocol_router().with_v1(Agent.builder()),
        "initialize",
        json!({}),
    )
    .await?;
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_rejection(&frames[0]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_initialize_requests_custom_finish_once_after_handoff() -> Result<(), Error> {
    for fail_flush in [false, true] {
        let (channel, Channel { mut rx, tx }) = Channel::duplex();
        let input_guard = tx;
        input_guard
            .unbounded_send(TransportFrame::Single(RawJsonRpcMessage::request(
                "initialize".into(),
                json!({}),
                v1::RequestId::Number(7),
            )?))
            .map_err(Error::into_internal_error)?;

        let (finish_tx, finish_rx) = oneshot::channel();
        let (flush_started_tx, flush_started_rx) = oneshot::channel();
        let (flush_release_tx, flush_release_rx) = oneshot::channel();
        let finish_calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = finish_calls.clone();
        let drained = Arc::new(AtomicBool::new(false));
        let driver_drained = drained.clone();
        let flush_error = Error::internal_error().data("rejection flush failed");
        let driver_error = flush_error.clone();
        let adapter = NormalizedAdapter {
            channel,
            driver: ConnectionDriver::with_finish(
                async move {
                    // Consume no output until finish. Sealing then draining
                    // proves the rejection was handed off before the hook.
                    finish_rx
                        .await
                        .expect("explicit cooperative finish request");
                    rx.close();
                    let mut frames = Vec::new();
                    while let Some(frame) = rx.next().await {
                        frames.push(frame_value(frame)?);
                    }
                    assert_eq!(frames.len(), 1, "{frames:?}");
                    assert_rejection(&frames[0]);
                    flush_started_tx.send(()).unwrap();
                    flush_release_rx.await.unwrap();
                    driver_drained.store(true, Ordering::SeqCst);
                    if fail_flush {
                        Err(driver_error)
                    } else {
                        Ok(())
                    }
                },
                move || {
                    callback_calls.fetch_add(1, Ordering::SeqCst);
                    finish_tx.send(()).expect("owned driver is still alive");
                },
            ),
        };
        let mut router = Box::pin(
            Agent
                .protocol_router()
                .with_v1(Agent.builder())
                .connect_to(adapter),
        );

        assert!(router.as_mut().now_or_never().is_none());
        assert_eq!(finish_calls.load(Ordering::SeqCst), 1);
        assert!(matches!(flush_started_rx.now_or_never(), Some(Ok(()))));
        assert!(!drained.load(Ordering::SeqCst), "flush is still gated");

        flush_release_tx.send(()).unwrap();
        let result = tokio::time::timeout(DEADLINE, router)
            .await
            .expect("rejection drain must not require remote input EOF");
        if fail_flush {
            assert_eq!(result, Err(flush_error));
        } else {
            result?;
        }
        assert_eq!(finish_calls.load(Ordering::SeqCst), 1);
        assert!(drained.load(Ordering::SeqCst), "owned drain was awaited");
        drop(input_guard);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn finite_agent_router_flushes_response_and_final_notification() -> Result<(), Error> {
    let frames = run_over_lines(
        Agent.protocol_router().with_v1(FiniteAgent),
        "initialize",
        serde_json::to_value(v1::InitializeRequest::new(ProtocolVersion::V1))
            .map_err(Error::into_internal_error)?,
    )
    .await?;
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[0]["id"], 7);
    assert_eq!(frames[0]["result"]["protocolVersion"], 1);
    assert!(frames[0].get("error").is_none(), "{frames:?}");
    assert_final(&frames[1]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn finite_proxy_router_flushes_response_and_final_notification() -> Result<(), Error> {
    let frames = run_over_lines(
        Proxy.protocol_router().with_v1(FiniteProxy),
        "_proxy/initialize",
        serde_json::to_value(InitializeProxyRequest::from(v1::InitializeRequest::new(
            ProtocolVersion::V1,
        )))
        .map_err(Error::into_internal_error)?,
    )
    .await?;
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[0]["id"], 7);
    assert_eq!(frames[0]["result"]["protocolVersion"], 1);
    assert!(frames[0].get("error").is_none(), "{frames:?}");
    assert_final(&frames[1]);
    Ok(())
}
