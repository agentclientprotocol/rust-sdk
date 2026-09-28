#![cfg(all(feature = "unstable_protocol_v2", feature = "unstable_mcp_over_acp"))]

use std::{sync::Mutex, time::Duration};

use agent_client_protocol::{
    Agent, Channel, Client, ConnectionTo, Error, Responder, RunWithConnectionTo, V2ConnectionTo,
    mcp_server::{McpOutcome, McpRequest, McpRequestContext, McpServer, McpService},
    schema::{ProtocolVersion, v2},
};
use futures::future::BoxFuture;
use serde_json::json;
use tokio::sync::oneshot;

struct OnDrop(Option<oneshot::Sender<()>>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        if let Some(done) = self.0.take() {
            let _ = done.send(());
        }
    }
}

struct CleanupService {
    started: Mutex<Option<oneshot::Sender<()>>>,
    cleanup_started: Mutex<Option<oneshot::Sender<()>>>,
    runner_woke: Mutex<Option<oneshot::Receiver<()>>>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
    dropped: Mutex<Option<oneshot::Sender<()>>>,
}

struct ShutdownRunner(oneshot::Sender<()>);

impl RunWithConnectionTo<Agent> for ShutdownRunner {
    async fn run_with_connection_to(self, cx: ConnectionTo<Agent>) -> Result<(), Error> {
        cx.shutdown_requested().await;
        let _ = self.0.send(());
        std::future::pending().await
    }
}

impl McpService<Agent> for CleanupService {
    fn execute(
        &self,
        _request: McpRequest,
        context: McpRequestContext<Agent>,
    ) -> BoxFuture<'static, Result<McpOutcome, Error>> {
        let started = self.started.lock().unwrap().take().unwrap();
        let cleanup_started = self.cleanup_started.lock().unwrap().take().unwrap();
        let runner_woke = self.runner_woke.lock().unwrap().take().unwrap();
        let release = self.release.lock().unwrap().take().unwrap();
        let dropped = self.dropped.lock().unwrap().take().unwrap();
        Box::pin(async move {
            let _drop = OnDrop(Some(dropped));
            let _ = started.send(());
            context.operation_cancellation().cancelled().await;
            runner_woke.await.map_err(Error::into_internal_error)?;
            let _ = cleanup_started.send(());
            let _ = release.await;
            Err(Error::request_cancelled())
        })
    }
}

#[derive(Clone, Copy)]
enum Shutdown {
    PeerEof,
    Foreground,
    UnrelatedTaskError,
}

async fn shutdown_joins_native_cleanup(shutdown: Shutdown) -> Result<(), Error> {
    tokio::time::timeout(Duration::from_secs(10), async move {
        let (started_tx, started_rx) = oneshot::channel();
        let (cleanup_started_tx, cleanup_started_rx) = oneshot::channel();
        let (runner_woke_tx, runner_woke_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let (dropped_tx, mut dropped_rx) = oneshot::channel();
        let (peer_stop_tx, peer_stop_rx) = oneshot::channel::<()>();
        let mut peer_stop_tx = Some(peer_stop_tx);
        let (client_stop_tx, client_stop_rx) = oneshot::channel::<()>();
        let (peer, client) = Channel::duplex();
        let agent = Agent
            .v2()
            .on_receive_request(
                async |request: v2::InitializeRequest,
                       responder: Responder<v2::InitializeResponse>,
                       _cx| {
                    responder.respond(v2::InitializeResponse::new(
                        request.protocol_version,
                        v2::Implementation::new("shutdown-agent", "1"),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async |request: v2::NewSessionRequest,
                       responder: Responder<v2::NewSessionResponse>,
                       cx: V2ConnectionTo<Client>| {
                    let [v2::McpServer::Acp(server)] = request.mcp_servers.as_slice() else {
                        panic!("expected native MCP server declaration");
                    };
                    let server_id = server.server_id.clone();
                    let request_connection = cx.clone();
                    cx.spawn(async move {
                        let request =
                            v2::MessageMcpRequest::new(server_id, "cleanup-probe", "tools/call")
                                .params(
                                    json!({
                                        "name": "probe",
                                        "_meta": {
                                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                                            "io.modelcontextprotocol/clientCapabilities": {}
                                        }
                                    })
                                    .as_object()
                                    .unwrap()
                                    .clone(),
                                );
                        let _result = request_connection.send_request(request).block_task().await;
                        Ok(())
                    })?;
                    responder.respond(v2::NewSessionResponse::new("shutdown-session"))
                },
                agent_client_protocol::on_receive_request!(),
            );
        let peer_task = tokio::spawn(agent.connect_with(peer, async move |_cx| {
            let _ = peer_stop_rx.await;
            Ok(())
        }));
        let client_task = tokio::spawn(Client.v2().connect_with(client, async move |cx| {
            cx.send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                v2::Implementation::new("shutdown-client", "1"),
            ))
            .block_task()
            .await?;
            let server = McpServer::new_service(
                CleanupService {
                    started: Mutex::new(Some(started_tx)),
                    cleanup_started: Mutex::new(Some(cleanup_started_tx)),
                    runner_woke: Mutex::new(Some(runner_woke_rx)),
                    release: Mutex::new(Some(release_rx)),
                    dropped: Mutex::new(Some(dropped_tx)),
                },
                "shutdown-test",
                ShutdownRunner(runner_woke_tx),
            );
            cx.build_session(std::env::current_dir().map_err(Error::into_internal_error)?)
                .with_mcp_server(server)?
                .start_session()
                .block_task()
                .await?;
            match shutdown {
                Shutdown::PeerEof => cx.incoming_closed().await,
                Shutdown::Foreground => {
                    let _ = client_stop_rx.await;
                }
                Shutdown::UnrelatedTaskError => {
                    let _ = client_stop_rx.await;
                    cx.spawn(async { Err(Error::internal_error().data("unrelated task failed")) })?;
                    std::future::pending::<()>().await;
                }
            }
            Ok(())
        }));

        started_rx.await.map_err(Error::into_internal_error)?;
        if matches!(shutdown, Shutdown::PeerEof) {
            let _ = peer_stop_tx.take().unwrap().send(());
        } else {
            let _ = client_stop_tx.send(());
        }
        cleanup_started_rx
            .await
            .map_err(Error::into_internal_error)?;
        assert!(
            !client_task.is_finished(),
            "connection discarded pending native cleanup"
        );
        assert!(matches!(
            dropped_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let _ = release_tx.send(());
        dropped_rx.await.map_err(Error::into_internal_error)?;
        let result = client_task.await.map_err(Error::into_internal_error)?;
        if matches!(shutdown, Shutdown::UnrelatedTaskError) {
            let error = result.expect_err("task failure must remain the primary connection error");
            assert!(
                error.to_string().contains("unrelated task failed"),
                "{error}"
            );
        } else {
            result?;
        }
        if !matches!(shutdown, Shutdown::PeerEof) {
            let _ = peer_stop_tx.take().unwrap().send(());
        }
        peer_task.await.map_err(Error::into_internal_error)??;
        Ok(())
    })
    .await
    .expect("native cleanup shutdown timed out")
}

#[tokio::test]
async fn native_cleanup_survives_clean_incoming_eof() -> Result<(), Error> {
    shutdown_joins_native_cleanup(Shutdown::PeerEof).await
}

#[tokio::test]
async fn native_cleanup_survives_foreground_return() -> Result<(), Error> {
    shutdown_joins_native_cleanup(Shutdown::Foreground).await
}

#[tokio::test]
async fn unrelated_task_error_waits_for_native_cleanup_and_preserves_error() -> Result<(), Error> {
    shutdown_joins_native_cleanup(Shutdown::UnrelatedTaskError).await
}
