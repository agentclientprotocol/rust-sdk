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

fn original_error() -> Error {
    Error::internal_error().data("local runner failed")
}

struct FailingRunner(Option<oneshot::Receiver<()>>);

impl RunWithConnectionTo<Agent> for FailingRunner {
    async fn run_with_connection_to(self, _: ConnectionTo<Agent>) -> Result<(), Error> {
        if let Some(release) = self.0 {
            release.await.unwrap();
        }
        Err(original_error())
    }
}

struct PendingRunner {
    dropped: Arc<AtomicBool>,
    request: Option<oneshot::Receiver<()>>,
    ack: Option<oneshot::Sender<()>>,
    completed: Option<Arc<AtomicBool>>,
}

impl Drop for PendingRunner {
    fn drop(&mut self) {
        if let Some(completed) = &self.completed {
            assert!(
                completed.load(Ordering::Acquire),
                "actual sibling runner dropped before owned async cleanup"
            );
        }
        self.dropped.store(true, Ordering::Release);
    }
}

impl RunWithConnectionTo<Agent> for PendingRunner {
    async fn run_with_connection_to(mut self, _: ConnectionTo<Agent>) -> Result<(), Error> {
        if let Some(request) = self.request.take() {
            request.await.unwrap();
            self.ack.take().unwrap().send(()).unwrap();
        }
        std::future::pending().await
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

struct CleanupService {
    started: Mutex<Option<oneshot::Sender<()>>>,
    cleaning: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
    runner: Mutex<Option<oneshot::Sender<()>>>,
    ack: Mutex<Option<oneshot::Receiver<()>>>,
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
        let ack = self.ack.lock().unwrap().take().unwrap();
        let completed = self.completed.clone();
        Box::pin(async move {
            started.send(()).unwrap();
            context.operation_cancellation().cancelled().await;
            assert!(
                context
                    .send_notification("notifications/progress", None)
                    .await
                    .is_err()
            );
            runner.send(()).unwrap();
            ack.await.unwrap();
            cleaning.send(()).unwrap();
            release.await.unwrap();
            completed.store(true, Ordering::Release);
            Err(Error::request_cancelled())
        })
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

async fn assert_live(
    peer: &mut Channel,
    server: &Value,
    context: &McpRequestContext<Agent>,
    id: &str,
) {
    assert!(!context.operation_cancellation().is_cancelled());
    context
        .send_notification("notifications/progress", None)
        .await
        .unwrap();
    loop {
        let message = raw(peer).await;
        if message["method"] == "mcp/message" {
            assert_eq!(message["params"]["requestId"], "subscription");
            break;
        }
        assert!(
            message.get("method").is_none(),
            "unexpected publication: {message}"
        );
    }
    request(peer, server, id, "tools/list");
    loop {
        let message = raw(peer).await;
        if message["id"] == id {
            assert_eq!(message["result"]["result"]["live"], true, "{message}");
            break;
        }
    }
}

async fn v1_scope_error(owned_cleanup: bool, foreground_success: bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (transport, mut peer) = Channel::duplex();
        let (live_tx, live_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (fail_tx, fail_rx) = oneshot::channel();
        let (finish_op_tx, finish_op_rx) = oneshot::channel();
        let (op_finished_tx, op_finished_rx) = oneshot::channel();
        let (returned_tx, mut returned_rx) = oneshot::channel();
        let (finish_tx, finish_rx) = oneshot::channel();
        let (started_tx, started_rx) = oneshot::channel();
        let (cleaning_tx, cleaning_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let (runner_tx, runner_rx) = oneshot::channel();
        let (ack_tx, ack_rx) = oneshot::channel();
        let completed = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let runner_dropped = dropped.clone();
        let runner_completed = completed.clone();
        let service_completed = completed.clone();
        let client = tokio::spawn(Client.builder().connect_with(transport, async move |cx| {
            let _live = cx
                .build_session_cwd()?
                .with_mcp_server(McpServer::new_service(
                    "live",
                    LiveService(Mutex::new(Some(live_tx))),
                    NullRun,
                ))?
                .block_task()
                .start_session()
                .await?;
            // The unrelated subscription is already executing before the local
            // runner can fail, including the immediate-failure case.
            ready_rx.await.unwrap();
            let scoped = cx
                .build_session_cwd()?
                .with_mcp_server(McpServer::new_service(
                    "pending",
                    CleanupService {
                        started: Mutex::new(Some(started_tx)),
                        cleaning: Mutex::new(Some(cleaning_tx)),
                        release: Mutex::new(Some(release_rx)),
                        runner: Mutex::new(Some(runner_tx)),
                        ack: Mutex::new(Some(ack_rx)),
                        completed: service_completed,
                    },
                    PendingRunner {
                        dropped: runner_dropped,
                        request: owned_cleanup.then_some(runner_rx),
                        ack: owned_cleanup.then_some(ack_tx),
                        completed: owned_cleanup.then_some(runner_completed),
                    },
                ))?
                .with_mcp_server(McpServer::new_service(
                    "failing",
                    LiveService(Mutex::new(None)),
                    FailingRunner(owned_cleanup.then_some(fail_rx)),
                ))?;
            let error = scoped
                .block_task()
                .run_until(async |_session| {
                    if foreground_success {
                        finish_op_rx.await.unwrap();
                        op_finished_tx.send(()).unwrap();
                        Ok(())
                    } else {
                        std::future::pending::<Result<(), Error>>().await
                    }
                })
                .await
                .unwrap_err();
            assert_eq!(error, original_error());
            returned_tx.send(()).unwrap();
            finish_rx.await.unwrap();
            Ok(())
        }));
        let live_setup = raw(&mut peer).await;
        let live_server = live_setup["params"]["mcpServers"][0]["serverId"].clone();
        send(
            &peer,
            json!({"jsonrpc":"2.0","id":live_setup["id"],"result":{"sessionId":"live"}}),
        );
        request(&peer, &live_server, "subscription", "_test/subscribe");
        let live_context = live_rx.await.unwrap();
        ready_tx.send(()).unwrap();
        if owned_cleanup {
            let setup = raw(&mut peer).await;
            let server = setup["params"]["mcpServers"][0]["serverId"].clone();
            send(
                &peer,
                json!({"jsonrpc":"2.0","id":setup["id"],"result":{"sessionId":"scoped"}}),
            );
            request(&peer, &server, "owned", "tools/list");
            started_rx.await.unwrap();
            fail_tx.send(()).unwrap();
            cleaning_rx.await.unwrap();
            assert!(matches!(
                returned_rx.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            assert!(!dropped.load(Ordering::Acquire));
            assert_live(&mut peer, &live_server, &live_context, "during").await;
            if foreground_success {
                finish_op_tx.send(()).unwrap();
                op_finished_rx.await.unwrap();
            }
            release_tx.send(()).unwrap();
        }
        returned_rx.await.unwrap();
        assert!(
            dropped.load(Ordering::Acquire),
            "sibling destructor must run before scope returns"
        );
        if owned_cleanup {
            assert!(completed.load(Ordering::Acquire));
        }
        assert_live(&mut peer, &live_server, &live_context, "after").await;
        finish_tx.send(()).unwrap();
        client.await.unwrap().unwrap();
    })
    .await
    .expect("v1 local runner error timed out");
}

#[tokio::test]
async fn immediate_v1_composed_runner_error_preserves_other_session() {
    v1_scope_error(false, false).await;
}

#[tokio::test]
async fn v1_composed_runner_error_drives_sibling_owned_cleanup_before_return() {
    v1_scope_error(true, false).await;
}

#[tokio::test]
async fn foreground_success_does_not_hide_observed_v1_runner_error() {
    v1_scope_error(true, true).await;
}

#[cfg(feature = "unstable_protocol_v2")]
#[tokio::test]
async fn immediate_v2_composed_startup_error_preserves_existing_mcp_admission() {
    use agent_client_protocol::schema::{ProtocolVersion, v2};

    tokio::time::timeout(Duration::from_secs(10), async {
        let (transport, mut peer) = Channel::duplex();
        let (returned_tx, returned_rx) = oneshot::channel();
        let (finish_tx, finish_rx) = oneshot::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let runner_dropped = dropped.clone();
        let client = tokio::spawn(Client.v2().connect_with(transport, async move |cx| {
            cx.send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2, v2::Implementation::new("scope-test", "1"),
            )).block_task().await?;
            let _live = cx.build_session_cwd()?
                .with_mcp_server(McpServer::new_service(
                    "live", LiveService(Mutex::new(None)), NullRun,
                ))?.start_session().block_task().await?;
            let error = cx.build_session_cwd()?
                .with_mcp_server(McpServer::new_service(
                    "pending", LiveService(Mutex::new(None)),
                    PendingRunner {
                        dropped: runner_dropped, request: None, ack: None, completed: None,
                    },
                ))?
                .with_mcp_server(McpServer::new_service(
                    "failing", LiveService(Mutex::new(None)), FailingRunner(None),
                ))?
                .start_session().block_task().await.unwrap_err();
            assert_eq!(error, original_error());
            returned_tx.send(()).unwrap();
            finish_rx.await.unwrap();
            Ok(())
        }));
        let initialize = raw(&mut peer).await;
        let initialized = v2::InitializeResponse::new(
            ProtocolVersion::V2, v2::Implementation::new("scope-agent", "1"),
        ).capabilities(v2::AgentCapabilities::new().session(
            v2::SessionCapabilities::new().mcp(
                v2::McpCapabilities::new().acp(v2::McpAcpCapabilities::new()),
            ),
        ));
        send(&peer, json!({"jsonrpc":"2.0","id":initialize["id"],"result":initialized}));
        let live_setup = raw(&mut peer).await;
        let live_server = live_setup["params"]["mcpServers"][0]["serverId"].clone();
        send(&peer, json!({"jsonrpc":"2.0","id":live_setup["id"],"result":v2::NewSessionResponse::new("live")}));
        returned_rx.await.unwrap();
        assert!(dropped.load(Ordering::Acquire));
        // The very next frame must be the existing registration's response, not
        // a session/new request from the failed prepublication setup.
        request(&peer, &live_server, "after-startup-error", "tools/list");
        let response = raw(&mut peer).await;
        assert_eq!(response["id"], "after-startup-error", "{response}");
        assert_eq!(response["result"]["result"]["live"], true, "{response}");
        finish_tx.send(()).unwrap();
        client.await.unwrap().unwrap();
    }).await.expect("v2 composed startup error timed out");
}

#[cfg(feature = "unstable_protocol_v2")]
#[tokio::test]
async fn attached_v2_composed_runner_error_remains_connection_fatal() {
    use agent_client_protocol::schema::{ProtocolVersion, v2};

    tokio::time::timeout(Duration::from_secs(10), async {
        let (transport, mut peer) = Channel::duplex();
        let (fail_tx, fail_rx) = oneshot::channel();
        let (attached_tx, attached_rx) = oneshot::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let runner_dropped = dropped.clone();
        let client = tokio::spawn(Client.v2().connect_with(transport, async move |cx| {
            cx.send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2, v2::Implementation::new("scope-test", "1"),
            )).block_task().await?;
            let _session = cx.build_session_cwd()?
                .with_mcp_server(McpServer::new_service(
                    "pending", LiveService(Mutex::new(None)),
                    PendingRunner {
                        dropped: runner_dropped, request: None, ack: None, completed: None,
                    },
                ))?
                .with_mcp_server(McpServer::new_service(
                    "failing", LiveService(Mutex::new(None)), FailingRunner(Some(fail_rx)),
                ))?
                .start_session().block_task().await?;
            // Neither unrelated infinite user work nor the foreground may be joined.
            cx.spawn(std::future::pending::<Result<(), Error>>())?;
            attached_tx.send(()).unwrap();
            std::future::pending::<Result<(), Error>>().await
        }));
        let initialize = raw(&mut peer).await;
        let initialized = v2::InitializeResponse::new(
            ProtocolVersion::V2, v2::Implementation::new("scope-agent", "1"),
        ).capabilities(v2::AgentCapabilities::new().session(
            v2::SessionCapabilities::new().mcp(
                v2::McpCapabilities::new().acp(v2::McpAcpCapabilities::new()),
            ),
        ));
        send(&peer, json!({"jsonrpc":"2.0","id":initialize["id"],"result":initialized}));
        let setup = raw(&mut peer).await;
        send(&peer, json!({"jsonrpc":"2.0","id":setup["id"],"result":v2::NewSessionResponse::new("attached")}));
        attached_rx.await.unwrap();
        fail_tx.send(()).unwrap();
        let error = client.await.unwrap().unwrap_err();
        assert_eq!(error.code, original_error().code);
        assert_eq!(error.data.unwrap()["data"], "local runner failed");
        assert!(dropped.load(Ordering::Acquire));
    }).await.expect("attached v2 runner failure must stop the connection");
}
