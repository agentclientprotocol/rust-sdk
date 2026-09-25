#![cfg(feature = "unstable_protocol_v2")]

//! Keep the tool runner unpolled during cancellation, while ACP still dispatches.
//! This proves that service completion alone cannot release request admission.

use std::{
    future::Future,
    pin::pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
    time::Duration,
};

use agent_client_protocol::{
    Agent, Client, ConnectionTo, Error, Responder, RunWithConnectionTo, V2ConnectionTo,
    mcp_server::{
        McpConnectionTo, McpOutcome, McpRequest, McpRequestContext, McpServer, McpService, McpTool,
    },
    schema::{ProtocolVersion, v2},
};
use futures::{FutureExt, future::BoxFuture, task::AtomicWaker};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::oneshot;

#[derive(Deserialize, Serialize, JsonSchema)]
struct Input {
    label: String,
}

#[derive(Default)]
struct Gate {
    paused: AtomicBool,
    waker: AtomicWaker,
}

impl Gate {
    fn release(&self) {
        self.paused.store(false, Ordering::Release);
        self.waker.wake();
    }
}

struct PausedRunner<R> {
    runner: R,
    gate: Arc<Gate>,
}

impl<R: RunWithConnectionTo<Agent>> RunWithConnectionTo<Agent> for PausedRunner<R> {
    async fn run_with_connection_to(self, cx: ConnectionTo<Agent>) -> Result<(), Error> {
        let mut running = pin!(self.runner.run_with_connection_to(cx));
        futures::future::poll_fn(|cx| {
            self.gate.waker.register(cx.waker());
            if self.gate.paused.load(Ordering::Acquire) {
                Poll::Pending
            } else {
                running.as_mut().poll(cx)
            }
        })
        .await
    }
}

struct ReleaseOnDrop(Arc<Gate>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct SignalOnDrop(Option<oneshot::Sender<()>>);
impl Drop for SignalOnDrop {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _sent = tx.send(());
        }
    }
}

struct ToolService<T> {
    tool: Arc<T>,
    finished: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl<T> McpService<Agent> for ToolService<T>
where
    T: McpTool<Agent, Input = Input, Output = String> + 'static,
{
    fn execute(
        &self,
        request: McpRequest,
        cx: McpRequestContext<Agent>,
    ) -> BoxFuture<'static, Result<McpOutcome, Error>> {
        let tool = self.tool.clone();
        let finished = self.finished.clone();
        Box::pin(async move {
            let input: Input =
                serde_json::from_value(request.params.expect("parameters")["arguments"].clone())
                    .map_err(Error::into_internal_error)?;
            let _finished = SignalOnDrop(
                (input.label == "held")
                    .then(|| finished.lock().unwrap().take())
                    .flatten(),
            );
            let result = tokio::select! {
                biased;
                () = cx.operation_cancellation().cancelled() => Err(Error::request_cancelled()),
                result = tool.call_tool(input, cx.connection().clone()) => result,
            }?;
            Ok(McpOutcome::Result(json!({
                "resultType": "complete",
                "content": [{"type":"text", "text":result}]
            })))
        })
    }
}

async fn exercise<T, R>(
    tool: T,
    runner: R,
    started: oneshot::Receiver<()>,
    dropped: oneshot::Receiver<()>,
) -> Result<(), Error>
where
    T: McpTool<Agent, Input = Input, Output = String> + 'static,
    R: RunWithConnectionTo<Agent> + 'static,
{
    let gate = Arc::new(Gate::default());
    let (finished_tx, finished_rx) = oneshot::channel();
    let (result_tx, result_rx) = oneshot::channel();
    let state = Arc::new(Mutex::new(Some((started, dropped, finished_rx, result_tx))));
    let agent_gate = gate.clone();
    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _cx| {
                responder.respond(v2::InitializeResponse::new(
                    request.protocol_version,
                    v2::Implementation::new("cleanup-agent", "1"),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::NewSessionRequest,
                        responder: Responder<v2::NewSessionResponse>,
                        cx: V2ConnectionTo<Client>| {
                let [v2::McpServer::Acp(server)] = request.mcp_servers.as_slice() else {
                    panic!("expected native declaration");
                };
                let server_id = server.server_id.clone();
                let (started, mut dropped, finished, result_tx) =
                    state.lock().unwrap().take().unwrap();
                let gate = agent_gate.clone();
                let work_cx = cx.clone();
                cx.spawn(async move {
                    let result =
                        async {
                            let _release_on_failure = ReleaseOnDrop(gate.clone());
                            let request =
                                |label: &str| {
                                    v2::MessageMcpRequest::new(
                            server_id.clone(), "same-logical-id", "tools/call",
                        ).params(json!({
                            "name":"tool", "arguments":{"label":label},
                            "_meta": {
                                "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                                "io.modelcontextprotocol/clientCapabilities":{}
                            }
                        }).as_object().unwrap().clone())
                                };
                            let held = work_cx.send_request(request("held"));
                            started.await.map_err(Error::into_internal_error)?;
                            gate.paused.store(true, Ordering::Release);
                            held.cancel()?;
                            finished.await.map_err(Error::into_internal_error)?;
                            let mut response = Box::pin(held.block_task());

                            assert!(
                                matches!(
                                    dropped.try_recv(),
                                    Err(oneshot::error::TryRecvError::Empty)
                                ),
                                "runner remains paused"
                            );
                            assert!(
                                response.as_mut().now_or_never().is_none(),
                                "cleanup precedes response"
                            );
                            let duplicate = work_cx
                                .send_request(request("duplicate"))
                                .block_task()
                                .await
                                .expect_err("ID remains admitted during cleanup");
                            assert_eq!(i32::from(duplicate.code), -32602);

                            gate.release();
                            let error = response.await.expect_err("cancelled operation");
                            assert_eq!(i32::from(error.code), -32800);
                            assert!(dropped.try_recv().is_ok(), "tool dropped before reply");
                            let healthy =
                                work_cx.send_request(request("after")).block_task().await?;
                            let v2::MessageMcpResponse::Result { result, .. } = healthy else {
                                panic!("ID reuse after cleanup should succeed");
                            };
                            assert_eq!(result["content"][0]["text"], "after");
                            Ok::<_, Error>(())
                        }
                        .await;
                    let _sent = result_tx.send(result);
                    Ok(())
                })?;
                responder.respond(v2::NewSessionResponse::new("cleanup-session"))
            },
            agent_client_protocol::on_receive_request!(),
        );
    tokio::time::timeout(
        Duration::from_secs(10),
        Client.v2().connect_with(agent, async move |cx| {
            cx.send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                v2::Implementation::new("cleanup-client", "1"),
            ))
            .block_task()
            .await?;
            let server = McpServer::new_service(
                ToolService {
                    tool: Arc::new(tool),
                    finished: Arc::new(Mutex::new(Some(finished_tx))),
                },
                "cleanup",
                PausedRunner { runner, gate },
            );
            cx.build_session(std::env::current_dir().map_err(Error::into_internal_error)?)
                .with_mcp_server(server)?
                .start_session()
                .block_task()
                .await?;
            result_rx.await.map_err(Error::into_internal_error)?
        }),
    )
    .await
    .expect("cleanup ownership regression timed out")
}

#[tokio::test]
async fn mutable_tool_cleanup_precedes_id_release() -> Result<(), Error> {
    let (started_tx, started_rx) = oneshot::channel();
    let (dropped_tx, dropped_rx) = oneshot::channel();
    let mut signals = Some((started_tx, dropped_tx));
    let (tool, runner) = agent_client_protocol::mcp_server::tool_fn_mut(
        "tool",
        "cleanup probe",
        async move |input: Input, _cx: McpConnectionTo<Agent>| {
            if input.label == "held" {
                let (started, dropped) = signals.take().unwrap();
                let _drop = SignalOnDrop(Some(dropped));
                let _sent = started.send(());
                std::future::pending::<()>().await;
            }
            Ok(input.label)
        },
        agent_client_protocol::tool_fn_mut!(),
    );
    exercise(tool, runner, started_rx, dropped_rx).await
}

#[tokio::test]
async fn concurrent_tool_cleanup_precedes_id_release() -> Result<(), Error> {
    let (started_tx, started_rx) = oneshot::channel();
    let (dropped_tx, dropped_rx) = oneshot::channel();
    let signals = Mutex::new(Some((started_tx, dropped_tx)));
    let (tool, runner) = agent_client_protocol::mcp_server::tool_fn(
        "tool",
        "cleanup probe",
        async move |input: Input, _cx: McpConnectionTo<Agent>| {
            if input.label == "held" {
                let (started, dropped) = signals.lock().unwrap().take().unwrap();
                let _drop = SignalOnDrop(Some(dropped));
                let _sent = started.send(());
                std::future::pending::<()>().await;
            }
            Ok(input.label)
        },
        agent_client_protocol::tool_fn!(),
    );
    exercise(tool, runner, started_rx, dropped_rx).await
}
