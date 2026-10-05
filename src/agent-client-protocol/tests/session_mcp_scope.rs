#![cfg(feature = "unstable_mcp_over_acp")]

use agent_client_protocol::{
    Agent, Channel, Client, ConnectionTo, Error, NullRun, RawJsonRpcMessage, RunWithConnectionTo,
    TransportFrame,
    mcp_server::{McpOutcome, McpRequest, McpRequestContext, McpServer, McpService},
};
use futures::{StreamExt, future::BoxFuture};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot;

struct CleanupService {
    started: Mutex<Option<oneshot::Sender<()>>>,
    cleaning: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
    runner: Mutex<Option<oneshot::Sender<()>>>,
    runner_ack: Mutex<Option<oneshot::Receiver<()>>>,
    completed: Arc<AtomicBool>,
}

impl McpService<Agent> for CleanupService {
    fn execute(
        &self,
        _: McpRequest,
        context: McpRequestContext<Agent>,
    ) -> BoxFuture<'static, Result<McpOutcome, Error>> {
        let started = self.started.lock().unwrap().take().unwrap();
        let cleaning = self.cleaning.lock().unwrap().take().unwrap();
        let release = self.release.lock().unwrap().take().unwrap();
        let runner = self.runner.lock().unwrap().take().unwrap();
        let runner_ack = self.runner_ack.lock().unwrap().take().unwrap();
        let completed = self.completed.clone();
        Box::pin(async move {
            let _ = started.send(());
            context.operation_cancellation().cancelled().await;
            assert!(
                context
                    .send_notification("notifications/progress", None)
                    .await
                    .is_err()
            );
            runner.send(()).unwrap();
            runner_ack.await.unwrap();
            let _ = cleaning.send(());
            release.await.unwrap();
            completed.store(true, Ordering::Release);
            Err(Error::request_cancelled())
        })
    }
}

struct CleanupRunner<'a> {
    request: Option<oneshot::Receiver<()>>,
    ack: Option<oneshot::Sender<()>>,
    completed: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
    borrowed: &'a str,
    drop_tx: Option<oneshot::Sender<()>>,
}

impl Drop for CleanupRunner<'_> {
    fn drop(&mut self) {
        assert!(
            self.completed.load(Ordering::Acquire),
            "actual scoped runner was dropped before operation cleanup"
        );
        assert_eq!(self.borrowed, "borrowed runner");
        self.dropped.store(true, Ordering::Release);
        if let Some(tx) = self.drop_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl RunWithConnectionTo<Agent> for CleanupRunner<'_> {
    async fn run_with_connection_to(mut self, _: ConnectionTo<Agent>) -> Result<(), Error> {
        self.request.take().unwrap().await.unwrap();
        let _ = self.ack.take().unwrap().send(());
        std::future::pending::<()>().await;
        Ok(())
    }
}

struct CleanupProbe {
    service: CleanupService,
    request: oneshot::Receiver<()>,
    ack: oneshot::Sender<()>,
    started: oneshot::Receiver<()>,
    cleaning: oneshot::Receiver<()>,
    release: oneshot::Sender<()>,
    completed: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

fn cleanup_probe() -> CleanupProbe {
    let (started_tx, started) = oneshot::channel();
    let (cleaning_tx, cleaning) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    let (runner, request) = oneshot::channel();
    let (ack, runner_ack) = oneshot::channel();
    let completed = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    CleanupProbe {
        service: CleanupService {
            started: Mutex::new(Some(started_tx)),
            cleaning: Mutex::new(Some(cleaning_tx)),
            release: Mutex::new(Some(release_rx)),
            runner: Mutex::new(Some(runner)),
            runner_ack: Mutex::new(Some(runner_ack)),
            completed: completed.clone(),
        },
        request,
        ack,
        started,
        cleaning,
        release,
        completed,
        dropped,
    }
}

struct LiveService(Mutex<Option<oneshot::Sender<McpRequestContext<Agent>>>>);

impl McpService<Agent> for LiveService {
    fn execute(
        &self,
        request: McpRequest,
        context: McpRequestContext<Agent>,
    ) -> BoxFuture<'static, Result<McpOutcome, Error>> {
        if request.method == "_test/subscribe" {
            self.0
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(context.clone())
                .unwrap();
            Box::pin(async move {
                context.operation_cancellation().cancelled().await;
                Err(Error::request_cancelled())
            })
        } else {
            Box::pin(async { Ok(McpOutcome::Result(json!({"live":true}))) })
        }
    }
}

fn send(peer: &Channel, value: Value) {
    let message: RawJsonRpcMessage = serde_json::from_value(value).unwrap();
    peer.tx
        .unbounded_send(TransportFrame::Single(message))
        .unwrap();
}

async fn raw(peer: &mut Channel) -> Value {
    let TransportFrame::Single(message) = peer.rx.next().await.unwrap() else {
        panic!("expected single frame")
    };
    serde_json::to_value(message).unwrap()
}

async fn response(peer: &mut Channel, id: &str) -> Value {
    loop {
        let message = raw(peer).await;
        if message["id"] == id {
            return message;
        }
    }
}

fn request(peer: &Channel, server: &Value, id: &str, method: &str) {
    send(
        peer,
        json!({"jsonrpc":"2.0","id":id,"method":"mcp/message","params":{
            "serverId":server,"requestId":id,"method":method,"params":{"_meta":{
                "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                "io.modelcontextprotocol/clientCapabilities":{}
            }}
        }}),
    );
}

#[derive(Clone, Copy)]
enum ScopeExit {
    Finished,
    Cancelled,
    SetupFailed,
}

async fn isolated_scope(exit: ScopeExit) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (transport, mut peer) = Channel::duplex();
        let CleanupProbe {
            service,
            request: runner_request,
            ack,
            started,
            cleaning,
            release,
            completed,
            dropped,
        } = cleanup_probe();
        let runner_completed = completed.clone();
        let runner_dropped = dropped.clone();
        let (live_tx, live_rx) = oneshot::channel();
        let (exit_tx, exit_rx) = oneshot::channel();
        let (returned_tx, mut returned_rx) = oneshot::channel();
        let (finish_tx, finish_rx) = oneshot::channel();
        let client = tokio::spawn(Client.builder().connect_with(transport, async move |cx| {
            // This registration and its subscription outlive the scoped session.
            let live_session = cx
                .build_session_cwd()?
                .with_mcp_server(McpServer::new_service(
                    "live",
                    LiveService(Mutex::new(Some(live_tx))),
                    NullRun,
                ))?
                .block_task()
                .start_session()
                .await?;
            let local = String::from("borrowed runner");
            let result = cx
                .build_session_cwd()?
                .with_mcp_server(McpServer::new_service(
                    "scoped",
                    service,
                    CleanupRunner {
                        request: Some(runner_request),
                        ack: Some(ack),
                        completed: runner_completed,
                        dropped: runner_dropped,
                        borrowed: &local,
                        drop_tx: None,
                    },
                ))?
                .block_task()
                .run_until(async |_session| {
                    exit_rx.await.unwrap();
                    match exit {
                        ScopeExit::Cancelled => Err(Error::request_cancelled()),
                        _ => Ok(()),
                    }
                })
                .await;
            let _sent = returned_tx.send(result);
            finish_rx.await.unwrap();
            drop(live_session);
            Ok(())
        }));

        let live_setup = raw(&mut peer).await;
        let live_server = live_setup["params"]["mcpServers"][0]["serverId"].clone();
        send(
            &peer,
            json!({"jsonrpc":"2.0","id":live_setup["id"],"result":{"sessionId":"live"}}),
        );
        let scoped_setup = raw(&mut peer).await;
        let scoped_server = scoped_setup["params"]["mcpServers"][0]["serverId"].clone();
        request(&peer, &live_server, "subscription", "_test/subscribe");
        let live_context = live_rx.await.unwrap();
        request(&peer, &scoped_server, "scoped-operation", "tools/list");
        started.await.unwrap();

        if matches!(exit, ScopeExit::SetupFailed) {
            send(
                &peer,
                json!({"jsonrpc":"2.0","id":scoped_setup["id"],
                "error":{"code":-32602,"message":"setup rejected"}}),
            );
        } else {
            send(
                &peer,
                json!({"jsonrpc":"2.0","id":scoped_setup["id"],"result":{"sessionId":"scoped"}}),
            );
            let _ = exit_tx.send(());
        }
        // Reached only after cleanup has handshaken with the actual borrowed runner.
        cleaning.await.unwrap();
        assert!(
            matches!(
                returned_rx.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ),
            "scope returned before cleanup"
        );
        assert!(!dropped.load(Ordering::Acquire));
        assert!(!completed.load(Ordering::Acquire));

        // During gated cleanup, other registrations retain both output and admission.
        live_context
            .send_notification("notifications/progress", None)
            .await
            .unwrap();
        assert_eq!(raw(&mut peer).await["params"]["requestId"], "subscription");
        request(&peer, &live_server, "during-cleanup", "tools/list");
        assert_eq!(
            response(&mut peer, "during-cleanup").await["result"]["result"]["live"],
            true
        );
        release.send(()).unwrap();
        let result = returned_rx.await.unwrap();
        match exit {
            ScopeExit::Finished => result.unwrap(),
            ScopeExit::Cancelled => {
                assert_eq!(result.unwrap_err().code, Error::request_cancelled().code);
            }
            ScopeExit::SetupFailed => {
                assert_eq!(result.unwrap_err().code, Error::invalid_params().code);
            }
        }
        assert!(completed.load(Ordering::Acquire));
        assert!(dropped.load(Ordering::Acquire));
        assert!(!live_context.operation_cancellation().is_cancelled());
        request(&peer, &live_server, "after-scope", "tools/list");
        assert_eq!(
            response(&mut peer, "after-scope").await["result"]["result"]["live"],
            true
        );
        let _ = finish_tx.send(());
        client.await.unwrap().unwrap();
    })
    .await
    .expect("isolated MCP scope timed out");
}

#[tokio::test]
async fn scope_joins_borrowed_runner_cleanup_without_closing_other_session() {
    isolated_scope(ScopeExit::Finished).await;
}

#[tokio::test]
async fn cancelled_foreground_joins_only_its_session_cleanup() {
    isolated_scope(ScopeExit::Cancelled).await;
}

#[tokio::test]
async fn rejected_setup_joins_borrowed_runner_cleanup() {
    isolated_scope(ScopeExit::SetupFailed).await;
}

#[cfg(feature = "unstable_protocol_v2")]
mod v2_setup {
    use super::*;
    use agent_client_protocol::schema::{ProtocolVersion, v2};

    #[derive(Clone, Copy)]
    enum Setup {
        New,
        Resume,
        #[cfg(feature = "unstable_session_fork")]
        Fork,
    }

    async fn failed_setup(setup: Setup, cancel: bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (transport, mut peer) = Channel::duplex();
            let CleanupProbe {
                service,
                request: runner_request,
                ack,
                started,
                cleaning,
                release,
                completed,
                dropped,
            } = cleanup_probe();
            let runner_completed = completed.clone();
            let runner_dropped = dropped.clone();
            let (live_tx, live_rx) = oneshot::channel();
            let (cancel_tx, cancel_rx) = oneshot::channel();
            let (drop_tx, drop_rx) = oneshot::channel();
            let (finish_tx, finish_rx) = oneshot::channel();
            let client = tokio::spawn(Client.v2().connect_with(transport, async move |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("scope-test", "1"),
                ))
                .block_task()
                .await?;
                let _live = cx
                    .build_session_cwd()?
                    .with_mcp_server(McpServer::new_service(
                        "live",
                        LiveService(Mutex::new(Some(live_tx))),
                        NullRun,
                    ))?
                    .start_session()
                    .block_task()
                    .await?;
                let server = McpServer::new_service(
                    "pending",
                    service,
                    CleanupRunner {
                        request: Some(runner_request),
                        ack: Some(ack),
                        completed: runner_completed,
                        dropped: runner_dropped,
                        borrowed: "borrowed runner",
                        drop_tx: Some(drop_tx),
                    },
                );
                let cwd = std::env::current_dir().map_err(Error::into_internal_error)?;
                let pending = match setup {
                    Setup::New => cx
                        .build_session(&cwd)
                        .with_mcp_server(server)?
                        .start_session()
                        .map(|_| Ok(())),
                    Setup::Resume => cx
                        .resume_session("existing", &cwd)
                        .with_mcp_server(server)?
                        .start_session()
                        .map(|_| Ok(())),
                    #[cfg(feature = "unstable_session_fork")]
                    Setup::Fork => cx
                        .fork_session("existing", &cwd)
                        .with_mcp_server(server)?
                        .start_session()
                        .map(|_| Ok(())),
                };
                if cancel {
                    cancel_rx.await.unwrap();
                    drop(pending);
                } else {
                    assert_eq!(
                        pending.block_task().await.unwrap_err().code,
                        Error::invalid_params().code
                    );
                }
                finish_rx.await.unwrap();
                Ok(())
            }));

            let initialize = raw(&mut peer).await;
            let initialized = v2::InitializeResponse::new(
                ProtocolVersion::V2,
                v2::Implementation::new("scope-agent", "1"),
            )
            .capabilities(
                v2::AgentCapabilities::new().session(
                    v2::SessionCapabilities::new()
                        .mcp(v2::McpCapabilities::new().acp(v2::McpAcpCapabilities::new())),
                ),
            );
            send(
                &peer,
                json!({"jsonrpc":"2.0","id":initialize["id"],"result":initialized}),
            );
            let live_setup = raw(&mut peer).await;
            let live_server = live_setup["params"]["mcpServers"][0]["serverId"].clone();
            let live_response = v2::NewSessionResponse::new("live");
            send(
                &peer,
                json!({"jsonrpc":"2.0","id":live_setup["id"],"result":live_response}),
            );
            let pending_setup = raw(&mut peer).await;
            let pending_server = pending_setup["params"]["mcpServers"][0]["serverId"].clone();
            request(&peer, &live_server, "subscription", "_test/subscribe");
            let live_context = live_rx.await.unwrap();
            request(&peer, &pending_server, "pending-operation", "tools/list");
            started.await.unwrap();
            if cancel {
                let _ = cancel_tx.send(());
                // Cancellation still requires the peer's terminal response.
                let cancellation = raw(&mut peer).await;
                assert_eq!(cancellation["method"], "$/cancel_request");
                send(
                    &peer,
                    json!({"jsonrpc":"2.0","id":pending_setup["id"],
                    "error":{"code":-32800,"message":"setup cancelled"}}),
                );
            } else {
                send(
                    &peer,
                    json!({"jsonrpc":"2.0","id":pending_setup["id"],
                    "error":{"code":-32602,"message":"setup rejected"}}),
                );
            }
            cleaning.await.unwrap();
            assert!(!dropped.load(Ordering::Acquire));
            assert!(!completed.load(Ordering::Acquire));
            request(&peer, &live_server, "during-cleanup", "tools/list");
            assert_eq!(
                response(&mut peer, "during-cleanup").await["result"]["result"]["live"],
                true
            );
            release.send(()).unwrap();
            drop_rx.await.unwrap();
            assert!(completed.load(Ordering::Acquire));
            assert!(dropped.load(Ordering::Acquire));
            assert!(!live_context.operation_cancellation().is_cancelled());
            request(&peer, &live_server, "after-cleanup", "tools/list");
            assert_eq!(
                response(&mut peer, "after-cleanup").await["result"]["result"]["live"],
                true
            );
            let _ = finish_tx.send(());
            client.await.unwrap().unwrap();
        })
        .await
        .expect("v2 pending setup cleanup timed out");
    }

    #[tokio::test]
    async fn new_setup_failure_and_cancellation_keep_runner_until_cleanup() {
        for cancel in [false, true] {
            failed_setup(Setup::New, cancel).await;
        }
    }

    #[tokio::test]
    async fn resume_setup_failure_and_cancellation_keep_runner_until_cleanup() {
        for cancel in [false, true] {
            failed_setup(Setup::Resume, cancel).await;
        }
    }

    #[cfg(feature = "unstable_session_fork")]
    #[tokio::test]
    async fn fork_setup_failure_and_cancellation_keep_runner_until_cleanup() {
        for cancel in [false, true] {
            failed_setup(Setup::Fork, cancel).await;
        }
    }
}
