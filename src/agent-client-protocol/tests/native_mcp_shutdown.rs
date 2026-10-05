#![cfg(feature = "unstable_mcp_over_acp")]

use agent_client_protocol::{
    Agent, Channel, Client, ConnectionTo, Error, RawJsonRpcMessage, RunWithConnectionTo,
    TransportFrame,
    mcp_server::{McpOutcome, McpRequest, McpRequestContext, McpServer, McpService},
};
use futures::{StreamExt, future::BoxFuture};
use serde_json::{Value, json};
use std::{sync::Mutex, time::Duration};
use tokio::sync::oneshot;

struct CleanupService {
    started: Mutex<Option<oneshot::Sender<()>>>,
    cleanup: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
    runner: Mutex<Option<oneshot::Sender<()>>>,
    runner_ack: Mutex<Option<oneshot::Receiver<()>>>,
}
impl McpService<Agent> for CleanupService {
    fn execute(
        &self,
        _: McpRequest,
        cx: McpRequestContext<Agent>,
    ) -> BoxFuture<'static, Result<McpOutcome, Error>> {
        let started = self.started.lock().unwrap().take().unwrap();
        let cleanup = self.cleanup.lock().unwrap().take().unwrap();
        let release = self.release.lock().unwrap().take().unwrap();
        let runner = self.runner.lock().unwrap().take().unwrap();
        let runner_ack = self.runner_ack.lock().unwrap().take().unwrap();
        Box::pin(async move {
            let _sent = started.send(());
            cx.operation_cancellation().cancelled().await;
            assert!(
                cx.send_notification("notifications/progress", None)
                    .await
                    .is_err()
            );
            // Cleanup depends on the real scoped runner still being driven.
            let _sent = runner.send(());
            runner_ack.await.map_err(Error::into_internal_error)?;
            let _sent = cleanup.send(());
            release.await.map_err(Error::into_internal_error)?;
            Err(Error::request_cancelled())
        })
    }
}
struct CleanupRunner {
    request: oneshot::Receiver<()>,
    ack: oneshot::Sender<()>,
}
impl RunWithConnectionTo<Agent> for CleanupRunner {
    async fn run_with_connection_to(self, _: ConnectionTo<Agent>) -> Result<(), Error> {
        self.request.await.map_err(Error::into_internal_error)?;
        let _sent = self.ack.send(());
        std::future::pending().await
    }
}

async fn raw(peer: &mut Channel) -> Value {
    let TransportFrame::Single(message) = peer.rx.next().await.expect("frame") else {
        panic!("expected a single frame")
    };
    serde_json::to_value(message).unwrap()
}
fn send(peer: &Channel, value: Value) {
    let message: RawJsonRpcMessage = serde_json::from_value(value).unwrap();
    peer.tx
        .unbounded_send(TransportFrame::Single(message))
        .unwrap();
}

#[derive(Clone, Copy)]
enum Stop {
    Eof,
    Foreground,
    TaskError,
}

async fn joined_cleanup(stop: Stop) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (transport, mut peer) = Channel::duplex();
        let (started_tx, started_rx) = oneshot::channel();
        let (cleanup_tx, cleanup_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let (runner_tx, runner_rx) = oneshot::channel();
        let (ack_tx, ack_rx) = oneshot::channel();
        let (stop_tx, stop_rx) = oneshot::channel();
        let service = CleanupService {
            started: Mutex::new(Some(started_tx)),
            cleanup: Mutex::new(Some(cleanup_tx)),
            release: Mutex::new(Some(release_rx)),
            runner: Mutex::new(Some(runner_tx)),
            runner_ack: Mutex::new(Some(ack_rx)),
        };
        let client = tokio::spawn(Client.builder().connect_with(transport, async move |cx| {
            cx.build_session_cwd()?
                .with_mcp_server(McpServer::new_service(
                    "cleanup",
                    service,
                    CleanupRunner {
                        request: runner_rx,
                        ack: ack_tx,
                    },
                ))?
                .block_task()
                .run_until(async |_session| {
                    if matches!(stop, Stop::Eof) {
                        cx.incoming_closed().await;
                    } else {
                        stop_rx.await.map_err(Error::into_internal_error)?;
                    }
                    if matches!(stop, Stop::TaskError) {
                        cx.spawn(async {
                            Err(Error::internal_error().data("unrelated task failed"))
                        })?;
                        std::future::pending::<()>().await;
                    }
                    Ok(())
                })
                .await?;
            Ok(())
        }));
        let setup = raw(&mut peer).await;
        let server = setup["params"]["mcpServers"][0]["serverId"].clone();
        send(
            &peer,
            json!({"jsonrpc":"2.0","id":setup["id"],"result":{"sessionId":"cleanup"}}),
        );
        send(
            &peer,
            json!({"jsonrpc":"2.0","id":"operation","method":"mcp/message","params":{
                "serverId":server,"requestId":"logical","method":"tools/list","params":{"_meta":{
                    "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities":{}
                }}
            }}),
        );
        started_rx.await.unwrap();
        if !matches!(stop, Stop::Eof) {
            let _sent = stop_tx.send(());
        }
        // EOF must close the actual input sender, not merely a clone.
        let peer = if matches!(stop, Stop::Eof) {
            drop(peer);
            None
        } else {
            Some(peer)
        };
        cleanup_rx.await.unwrap();
        assert!(!client.is_finished(), "connection abandoned owned cleanup");
        let _sent = release_tx.send(());
        let result = client.await.unwrap();
        if matches!(stop, Stop::TaskError) {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("unrelated task failed")
            );
        } else {
            result.unwrap();
        }
        drop(peer);
    })
    .await
    .expect("native cleanup timed out");
}

#[tokio::test]
async fn eof_joins_native_cleanup_and_keeps_scoped_runner_live() {
    joined_cleanup(Stop::Eof).await;
}
#[tokio::test]
async fn foreground_completion_joins_native_cleanup() {
    joined_cleanup(Stop::Foreground).await;
}
#[tokio::test]
async fn task_error_joins_native_cleanup_without_replacing_primary_error() {
    joined_cleanup(Stop::TaskError).await;
}
