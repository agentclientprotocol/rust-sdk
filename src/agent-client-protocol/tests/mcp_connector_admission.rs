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
    Agent, ByteStreams, Channel, Client, ConnectTo, ConnectionLimits, ConnectionTo, DynConnectTo,
    Error, FrameSender, RawJsonRpcMessage, Responder, RunWithConnectionTo, TransportFrame,
    mcp_server::{McpConnectionTo, McpServer, McpServerConnect},
    role,
    schema::v1,
};
use futures::StreamExt as _;
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex},
    sync::oneshot,
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Probes {
    factory: AtomicUsize,
    backend_started: AtomicUsize,
    backend_dropped: AtomicUsize,
    escaped_senders: Mutex<Vec<FrameSender>>,
    notifications: Mutex<Vec<String>>,
}

#[derive(Clone, Copy)]
enum BackendBehavior {
    WireReply,
    ReplyThenError,
    ExitWithoutReply,
}

struct Connector(Arc<Probes>, BackendBehavior);

impl McpServerConnect<Agent> for Connector {
    fn name(&self) -> String {
        "capacity-probe".into()
    }

    fn connect(&self, context: McpConnectionTo<Agent>) -> DynConnectTo<role::mcp::Client> {
        assert!(context.request_id().is_some());
        self.0.factory.fetch_add(1, Ordering::SeqCst);
        DynConnectTo::new(Backend(self.0.clone(), self.1))
    }
}

struct Backend(Arc<Probes>, BackendBehavior);

struct BackendDrop(Arc<Probes>);

impl Drop for BackendDrop {
    fn drop(&mut self) {
        self.0.backend_dropped.fetch_add(1, Ordering::SeqCst);
    }
}

impl ConnectTo<role::mcp::Client> for Backend {
    async fn connect_to(self, client: impl ConnectTo<role::mcp::Server>) -> Result<(), Error> {
        self.0.backend_started.fetch_add(1, Ordering::SeqCst);
        let _drop = BackendDrop(self.0.clone());
        if !matches!(self.1, BackendBehavior::WireReply) {
            let (mut channel, driver) = client.into_channel_and_future();
            let work = async {
                let frame = channel.rx.next().await.expect("MCP request");
                let TransportFrame::Single(RawJsonRpcMessage::Request(request)) =
                    frame.into_frame()
                else {
                    panic!("expected one request");
                };
                self.0
                    .escaped_senders
                    .lock()
                    .unwrap()
                    .push(channel.tx.clone());
                if matches!(self.1, BackendBehavior::ExitWithoutReply) {
                    return Ok(());
                }
                for message in [
                    RawJsonRpcMessage::notification(
                        "notifications/progress".into(),
                        json!({"progressToken":1, "progress":1, "marker":"before"}),
                    )?,
                    RawJsonRpcMessage::response(request.id, Ok(json!({"admitted":true}))),
                    RawJsonRpcMessage::notification(
                        "notifications/progress".into(),
                        json!({"progressToken":1, "progress":2, "marker":"after"}),
                    )?,
                ] {
                    channel
                        .tx
                        .try_send(TransportFrame::Single(message))
                        .map_err(Error::into_internal_error)?;
                }
                Err(Error::internal_error().data("driver failed after accepted output"))
            };
            let (driver, result) = tokio::join!(driver, work);
            driver?;
            return result;
        }
        let (sdk_output, peer_input) = duplex(4096);
        let (peer_output, sdk_input) = duplex(4096);
        let transport = ByteStreams::new(sdk_output.compat_write(), sdk_input.compat());
        let peer = async move {
            let mut reader = BufReader::new(peer_input);
            let mut line = String::new();
            if reader.read_line(&mut line).await.expect("read MCP request") == 0 {
                // Rejected admissions close the backend before sending anything.
                return;
            }
            let request: Value = serde_json::from_str(&line).expect("valid MCP request");
            assert_eq!(
                request["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
                "2026-07-28"
            );
            let response = json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {"admitted": true}
            });
            let mut output = peer_output;
            output
                .write_all(format!("{response}\n").as_bytes())
                .await
                .expect("write MCP response");
            output.shutdown().await.expect("close MCP backend output");
        };
        let (result, ()) = tokio::join!(client.connect_to(transport), peer);
        result
    }
}

struct NullRun;

impl RunWithConnectionTo<Agent> for NullRun {
    async fn run_with_connection_to(self, _connection: ConnectionTo<Agent>) -> Result<(), Error> {
        pending().await
    }
}

// Unlike a raw Channel, this uses ConnectTo's default adapter. Only the
// provider endpoint is limited; the agent must have its own default pool.
struct DefaultCapacityAgent(Channel);

impl ConnectTo<Agent> for DefaultCapacityAgent {
    async fn connect_to(self, agent: impl ConnectTo<Client>) -> Result<(), Error> {
        ConnectTo::<Agent>::connect_to(self.0, agent).await
    }
}

fn params() -> Map<String, Value> {
    serde_json::from_value(json!({
        "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
            "progressToken": 1
        }
    }))
    .unwrap()
}

// Occupy slots only after newSession has completed. The agent waits on `start`
// so no MCP request can race the provider's admission setup.
async fn scenario(
    fillers: usize,
    requests: usize,
    behavior: BackendBehavior,
) -> (
    Vec<(Result<v1::MessageMcpResponse, Error>, usize)>,
    Arc<Probes>,
) {
    tokio::time::timeout(TIMEOUT, async move {
        let probes = Arc::new(Probes::default());
        let (provider_channel, agent_channel) = Channel::duplex_with_limits(ConnectionLimits {
            max_queued_frames: 8,
            ..ConnectionLimits::default()
        });
        let (start_tx, start_rx) = oneshot::channel::<()>();
        let start_rx = Mutex::new(Some(start_rx));
        let (done_tx, done_rx) = oneshot::channel();
        let done_tx = Mutex::new(Some(done_tx));
        let agent_probes = probes.clone();
        let notification_probes = probes.clone();
        let agent = Agent.builder().on_receive_request(
            async move |request: v1::NewSessionRequest,
                        responder: Responder<v1::NewSessionResponse>,
                        connection: ConnectionTo<Client>| {
                let [v1::McpServer::Acp(server)] = request.mcp_servers.as_slice() else {
                    panic!("expected one native MCP server")
                };
                let server_id = server.server_id.clone();
                responder.respond(v1::NewSessionResponse::new(v1::SessionId::new(
                    "capacity-session",
                )))?;
                let start = start_rx.lock().unwrap().take().expect("one session");
                let done = done_tx.lock().unwrap().take().expect("one session");
                let sender = connection.clone();
                let observed = agent_probes.clone();
                connection.spawn(async move {
                    start.await.map_err(Error::into_internal_error)?;
                    let mut responses = Vec::new();
                    for _ in 0..requests {
                        // Reuse the logical ID after the first request completes.
                        let response = sender
                            .send_request(
                                v1::MessageMcpRequest::new(
                                    server_id.clone(),
                                    "reused-id",
                                    "admission/probe",
                                )
                                .params(params()),
                            )
                            .block_task()
                            .await;
                        responses.push((response, observed.backend_dropped.load(Ordering::SeqCst)));
                    }
                    drop(done.send(responses));
                    Ok(())
                })
            },
            agent_client_protocol::on_receive_request!(),
        );
        let agent = agent.on_receive_notification(
            async move |notification: v1::MessageMcpNotification, _cx| {
                assert_eq!(notification.request_id, v1::McpRequestId::new("reused-id"));
                notification_probes.notifications.lock().unwrap().push(
                    notification.params.as_ref().unwrap()["marker"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                );
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        );
        let connector = Connector(probes.clone(), behavior);
        let test = Client
            .builder()
            .connect_with(provider_channel, async move |connection| {
                let filler_connection = connection.clone();
                connection
                    .build_session_cwd()?
                    .with_mcp_server(McpServer::<Agent, _>::new(connector, NullRun))?
                    .block_task()
                    .run_until(async |_session| {
                        for _ in 0..fillers {
                            filler_connection
                                .spawn(async { pending::<Result<(), Error>>().await })?;
                        }
                        start_tx.send(()).expect("agent still waiting");
                        let responses = done_rx.await.map_err(Error::into_internal_error)?;
                        Ok(responses)
                    })
                    .await
            });
        let (result, agent_result) =
            tokio::join!(test, DefaultCapacityAgent(agent_channel).connect_to(agent));
        agent_result.expect("agent connection");
        (result.expect("provider connection"), probes)
    })
    .await
    .expect("MCP capacity scenario timed out")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_free_slot_completes_connector_and_recovers_for_reused_id() {
    let (responses, probes) = scenario(7, 2, BackendBehavior::WireReply).await;
    for (index, (response, dropped_at_response)) in responses.into_iter().enumerate() {
        match response.expect("one slot must admit the MCP request") {
            v1::MessageMcpResponse::Result { result, .. } => {
                assert_eq!(result, json!({"admitted": true}));
            }
            other => panic!("expected MCP result, got {other:?}"),
        }
        assert_eq!(
            dropped_at_response,
            index + 1,
            "backend must stop before the logical response is observed"
        );
    }
    assert_eq!(probes.factory.load(Ordering::SeqCst), 2);
    assert_eq!(probes.backend_started.load(Ordering::SeqCst), 2);
    assert_eq!(probes.backend_dropped.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_free_slots_rejects_before_factory_or_backend_setup() {
    let (responses, probes) = scenario(8, 1, BackendBehavior::WireReply).await;
    let [(response, _)] = <[_; 1]>::try_from(responses).expect("one request");
    let error = response.expect_err("all live slots are occupied");
    assert!(error.to_string().contains("live task capacity"), "{error}");
    assert_eq!(probes.factory.load(Ordering::SeqCst), 0);
    assert_eq!(probes.backend_started.load(Ordering::SeqCst), 0);
    assert_eq!(probes.backend_dropped.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn connector_drains_terminal_output_before_driver_failure_and_rejects_late_output() {
    let (responses, probes) = scenario(7, 1, BackendBehavior::ReplyThenError).await;
    let [(response, dropped)] = <[_; 1]>::try_from(responses).unwrap();
    let v1::MessageMcpResponse::Result { result, .. } = response.unwrap() else {
        panic!("accepted terminal result must survive backend exit");
    };
    assert_eq!(result, json!({"admitted":true}));
    assert_eq!(dropped, 1);
    assert_eq!(*probes.notifications.lock().unwrap(), ["before"]);
    let escaped = probes.escaped_senders.lock().unwrap();
    assert_eq!(escaped.len(), 1);
    assert!(escaped[0].is_closed());
}

#[tokio::test]
async fn connector_exit_without_response_does_not_wait_for_escaped_sender() {
    let (responses, probes) = scenario(7, 1, BackendBehavior::ExitWithoutReply).await;
    let [(response, dropped)] = <[_; 1]>::try_from(responses).unwrap();
    let error = response.expect_err("completed backend did not produce a terminal outcome");
    assert_eq!(
        i32::from(error.code),
        agent_client_protocol::mcp_server::MCP_BACKEND_FAILURE
    );
    assert_eq!(dropped, 1);
    let escaped = probes.escaped_senders.lock().unwrap();
    assert_eq!(escaped.len(), 1);
    assert!(escaped[0].is_closed());
}
