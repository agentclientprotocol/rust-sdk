#![cfg(all(feature = "unstable_protocol_v2", feature = "unstable_mcp_over_acp"))]

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use agent_client_protocol::{
    Agent, Client, ConnectTo, ConnectionTo, DynConnectTo, Error, ErrorCode, JsonRpcNotification,
    JsonRpcRequest, JsonRpcResponse, Responder, RunWithConnectionTo, V2ConnectionTo,
    mcp_server::{McpConnectionTo, McpServer, McpServerConnect},
    role,
    schema::{ProtocolVersion, v2},
};
use futures::{
    StreamExt as _,
    channel::{
        mpsc::{self, UnboundedReceiver},
        oneshot,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

const TIMEOUT: Duration = Duration::from_secs(10);

fn cwd() -> Result<PathBuf, Error> {
    std::env::current_dir().map_err(Error::into_internal_error)
}

fn implementation() -> v2::Implementation {
    v2::Implementation::new("session-v2-mcp-test", env!("CARGO_PKG_VERSION"))
}

fn initialize_response(protocol_version: ProtocolVersion) -> v2::InitializeResponse {
    v2::InitializeResponse::new(protocol_version, implementation()).capabilities(
        v2::AgentCapabilities::new().session(
            v2::SessionCapabilities::new()
                .mcp(v2::McpCapabilities::new().acp(v2::McpAcpCapabilities::new())),
        ),
    )
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(value) => value,
        other => panic!("expected an object, got {other:?}"),
    }
}

async fn next<T>(receiver: &mut UnboundedReceiver<T>, description: &str) -> T {
    receiver
        .next()
        .await
        .unwrap_or_else(|| panic!("{description} channel closed unexpectedly"))
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_test/echo", response = EchoResponse)]
struct EchoRequest {
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
struct EchoResponse {
    echoed: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(
    method = "_test/connection-probe",
    response = ConnectionProbeResponse
)]
struct ConnectionProbeRequest {
    nonce: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
struct ConnectionProbeResponse {
    nonce: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcNotification)]
#[notification(method = "_test/notice")]
struct NoticeNotification {
    message: String,
}

#[derive(Debug, PartialEq, Eq)]
struct ObservedMcpContext {
    server_id: String,
    connection_id: String,
}

struct EchoMcpConnect {
    context_tx: mpsc::UnboundedSender<ObservedMcpContext>,
    notice_tx: mpsc::UnboundedSender<String>,
    runner_started: Arc<AtomicBool>,
    dropped_tx: Mutex<Option<oneshot::Sender<()>>>,
}

impl Drop for EchoMcpConnect {
    fn drop(&mut self) {
        if let Some(dropped_tx) = self
            .dropped_tx
            .get_mut()
            .expect("MCP connector drop mutex poisoned")
            .take()
        {
            let _ = dropped_tx.send(());
        }
    }
}

impl McpServerConnect<Agent> for EchoMcpConnect {
    fn name(&self) -> String {
        "v2-echo".to_owned()
    }

    fn connect(&self, context: McpConnectionTo<Agent>) -> DynConnectTo<role::mcp::Client> {
        assert!(
            self.runner_started.load(Ordering::Acquire),
            "the MCP runner must be first-polled before the agent can connect"
        );
        self.context_tx
            .unbounded_send(ObservedMcpContext {
                server_id: context
                    .server_id()
                    .expect("the MCP server should be attached through ACP")
                    .to_string(),
                connection_id: context
                    .connection_id()
                    .expect("an attached MCP connection should have an ID")
                    .to_string(),
            })
            .expect("MCP context receiver should remain active");

        DynConnectTo::new(EchoMcpComponent {
            notice_tx: self.notice_tx.clone(),
        })
    }
}

struct EchoMcpComponent {
    notice_tx: mpsc::UnboundedSender<String>,
}

impl ConnectTo<role::mcp::Client> for EchoMcpComponent {
    async fn connect_to(self, client: impl ConnectTo<role::mcp::Server>) -> Result<(), Error> {
        let notice_tx = self.notice_tx;

        role::mcp::Server
            .builder()
            .on_receive_notification(
                async move |notification: NoticeNotification, _connection| {
                    notice_tx
                        .unbounded_send(notification.message)
                        .map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async |request: EchoRequest, responder: Responder<EchoResponse>, _connection| {
                    responder.respond(EchoResponse {
                        echoed: request.message,
                    })
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}

struct ProbeRunner {
    started: Arc<AtomicBool>,
    started_tx: Option<oneshot::Sender<()>>,
    dropped_tx: Option<oneshot::Sender<()>>,
}

impl Drop for ProbeRunner {
    fn drop(&mut self) {
        if let Some(dropped_tx) = self.dropped_tx.take() {
            let _ = dropped_tx.send(());
        }
    }
}

impl RunWithConnectionTo<Agent> for ProbeRunner {
    async fn run_with_connection_to(
        mut self,
        _connection: ConnectionTo<Agent>,
    ) -> Result<(), Error> {
        self.started.store(true, Ordering::Release);
        if let Some(started_tx) = self.started_tx.take() {
            let _ = started_tx.send(());
        }
        std::future::pending::<()>().await;
        Ok(())
    }
}

#[derive(Debug)]
struct RoundTrip {
    server_id: String,
    connection_id: String,
    notice: String,
    response: Value,
}

async fn run_mcp_round_trip(
    connection: &V2ConnectionTo<Client>,
    server_id: &v2::McpServerAcpId,
    sequence: usize,
) -> Result<RoundTrip, Error> {
    let connected = connection
        .send_request(v2::ConnectMcpRequest::new(server_id.clone()))
        .block_task()
        .await?;
    let connection_id = connected.connection_id;
    let notice = format!("notice-{sequence}");
    connection.send_notification(
        v2::MessageMcpNotification::new(connection_id.clone(), "_test/notice")
            .params(object(json!({ "message": notice }))),
    )?;

    let message = format!("message-{sequence}");
    let response = connection
        .send_request(
            v2::MessageMcpRequest::new(connection_id.clone(), "_test/echo")
                .params(object(json!({ "message": message }))),
        )
        .block_task()
        .await?;
    let response = serde_json::from_str(response.0.get()).map_err(Error::into_internal_error)?;

    connection
        .send_request(v2::DisconnectMcpRequest::new(connection_id.clone()))
        .block_task()
        .await?;

    Ok(RoundTrip {
        server_id: server_id.to_string(),
        connection_id: connection_id.to_string(),
        notice,
        response,
    })
}

async fn assert_round_trip(
    sequence: usize,
    round_trip_rx: &mut UnboundedReceiver<Result<RoundTrip, Error>>,
    context_rx: &mut UnboundedReceiver<ObservedMcpContext>,
    notice_rx: &mut UnboundedReceiver<String>,
) -> Result<(), Error> {
    let round_trip = next(round_trip_rx, "MCP round trip").await?;
    let context = next(context_rx, "MCP connection context").await;
    let notice = next(notice_rx, "inner MCP notification").await;

    assert_eq!(context.server_id, round_trip.server_id);
    assert_eq!(context.connection_id, round_trip.connection_id);
    assert_eq!(notice, round_trip.notice);
    assert_eq!(
        round_trip.response,
        json!({ "echoed": format!("message-{sequence}") })
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_session_mcp_attachment_is_ready_during_setup_and_lives_for_connection()
-> Result<(), Error> {
    let (server_id_tx, mut server_id_rx) = mpsc::unbounded();
    let (round_trip_trigger_tx, mut round_trip_trigger_rx) = mpsc::unbounded();
    let (round_trip_tx, mut round_trip_rx) = mpsc::unbounded();
    let first_round_trip_tx = round_trip_tx.clone();
    let existing_mcp_server = v2::McpServer::Other(v2::OtherMcpServer::new(
        "_test_transport",
        BTreeMap::from([("extension".to_owned(), json!({"preserved": true}))]),
    ));
    let expected_existing_mcp_server = existing_mcp_server.clone();
    let setup_meta = Map::from_iter([("setup".to_owned(), json!({"preserved": true}))]);
    let expected_setup_meta = setup_meta.clone();

    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(initialize_response(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::NewSessionRequest,
                        responder: Responder<v2::NewSessionResponse>,
                        connection: V2ConnectionTo<Client>| {
                assert_eq!(request.meta.as_ref(), Some(&expected_setup_meta));
                let server = match request.mcp_servers.as_slice() {
                    [existing, v2::McpServer::Acp(server)]
                        if existing == &expected_existing_mcp_server =>
                    {
                        server
                    }
                    servers => {
                        panic!("expected the existing declaration followed by ACP, got {servers:?}")
                    }
                };
                assert_eq!(server.name, "v2-echo");
                let server_id = server.server_id.clone();
                server_id_tx
                    .unbounded_send(server_id.clone())
                    .map_err(Error::into_internal_error)?;
                let round_trip_connection = connection.clone();
                let first_round_trip_tx = first_round_trip_tx.clone();
                connection.spawn(async move {
                    match run_mcp_round_trip(&round_trip_connection, &server_id, 1).await {
                        Ok(round_trip) => {
                            first_round_trip_tx
                                .unbounded_send(Ok(round_trip))
                                .map_err(Error::into_internal_error)?;
                            responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                                "v2-mcp-session",
                            )))
                        }
                        Err(error) => {
                            first_round_trip_tx
                                .unbounded_send(Err(error.clone()))
                                .map_err(Error::into_internal_error)?;
                            responder.respond_with_error(error)
                        }
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .with_spawned(move |connection: V2ConnectionTo<Client>| async move {
            let server_id = server_id_rx.next().await.ok_or_else(|| {
                Error::internal_error().data("session/new did not advertise an MCP server")
            })?;
            let mut sequence = 1;

            while round_trip_trigger_rx.next().await.is_some() {
                sequence += 1;
                let result = run_mcp_round_trip(&connection, &server_id, sequence).await;
                let failed = result.is_err();
                round_trip_tx
                    .unbounded_send(result)
                    .map_err(Error::into_internal_error)?;
                if failed {
                    break;
                }
            }
            Ok(())
        });

    let test = async move {
        let (context_tx, mut context_rx) = mpsc::unbounded();
        let (notice_tx, mut notice_rx) = mpsc::unbounded();
        let (connector_dropped_tx, connector_dropped_rx) = oneshot::channel();
        let (runner_started_tx, runner_started_rx) = oneshot::channel();
        let (runner_dropped_tx, runner_dropped_rx) = oneshot::channel();
        let runner_started = Arc::new(AtomicBool::new(false));

        Client
            .v2()
            .connect_with(agent, async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        implementation(),
                    ))
                    .block_task()
                    .await?;

                let mcp_server = McpServer::<Agent, _>::new(
                    EchoMcpConnect {
                        context_tx,
                        notice_tx,
                        runner_started: runner_started.clone(),
                        dropped_tx: Mutex::new(Some(connector_dropped_tx)),
                    },
                    ProbeRunner {
                        started: runner_started.clone(),
                        started_tx: Some(runner_started_tx),
                        dropped_tx: Some(runner_dropped_tx),
                    },
                );
                let pending_session = connection
                    .build_session_from(
                        v2::NewSessionRequest::new(cwd()?)
                            .mcp_servers(vec![existing_mcp_server])
                            .meta(setup_meta),
                    )
                    .with_mcp_server(mcp_server)?
                    .start_session();

                runner_started_rx
                    .await
                    .map_err(Error::into_internal_error)?;
                assert!(
                    runner_started.load(Ordering::Acquire),
                    "the MCP runner must be first-polled before session/new is published"
                );

                assert_round_trip(1, &mut round_trip_rx, &mut context_rx, &mut notice_rx).await?;

                let session = pending_session.block_task().await?.into_session();
                let remaining_session = session.clone();
                drop(session);
                drop(remaining_session);

                round_trip_trigger_tx
                    .unbounded_send(())
                    .map_err(Error::into_internal_error)?;
                assert_round_trip(2, &mut round_trip_rx, &mut context_rx, &mut notice_rx).await?;

                Ok(())
            })
            .await?;

        connector_dropped_rx
            .await
            .map_err(Error::into_internal_error)?;
        runner_dropped_rx.await.map_err(Error::into_internal_error)
    };

    tokio::time::timeout(TIMEOUT, test)
        .await
        .expect("v2 MCP attachment test timed out")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_resume_mcp_attachment_preserves_request_and_lives_for_connection() -> Result<(), Error>
{
    let (server_id_tx, mut server_id_rx) = mpsc::unbounded();
    let (round_trip_trigger_tx, mut round_trip_trigger_rx) = mpsc::unbounded();
    let (round_trip_tx, mut round_trip_rx) = mpsc::unbounded();
    let first_round_trip_tx = round_trip_tx.clone();
    let session_id = v2::SessionId::new("v2-resumed-mcp-session");
    let expected_session_id = session_id.clone();
    let resume_cwd = cwd()?;
    let expected_resume_cwd = v2::AbsolutePath::new(resume_cwd.clone());
    let additional_directory = resume_cwd.join("additional");
    let expected_additional_directory = v2::AbsolutePath::new(additional_directory.clone());
    let existing_mcp_server = v2::McpServer::Other(v2::OtherMcpServer::new(
        "_test_transport",
        BTreeMap::from([("extension".to_owned(), json!({"preserved": true}))]),
    ));
    let expected_existing_mcp_server = existing_mcp_server.clone();
    let replay_meta = Map::from_iter([("replay".to_owned(), json!({"preserved": true}))]);
    let expected_replay_meta = replay_meta.clone();
    let setup_meta = Map::from_iter([("setup".to_owned(), json!({"preserved": true}))]);
    let expected_setup_meta = setup_meta.clone();
    let expected_response = v2::ResumeSessionResponse::new().meta(Map::from_iter([(
        "response".to_owned(),
        json!({"preserved": true}),
    )]));
    let agent_response = expected_response.clone();

    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(initialize_response(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::ResumeSessionRequest,
                        responder: Responder<v2::ResumeSessionResponse>,
                        connection: V2ConnectionTo<Client>| {
                assert_eq!(request.session_id, expected_session_id);
                assert_eq!(request.cwd, expected_resume_cwd);
                assert_eq!(
                    request.additional_directories,
                    vec![expected_additional_directory.clone()]
                );
                assert!(matches!(
                    request.replay_from.as_ref(),
                    Some(v2::ReplayFrom::Start(start))
                        if start.meta.as_ref() == Some(&expected_replay_meta)
                ));
                assert_eq!(request.meta.as_ref(), Some(&expected_setup_meta));
                let server = match request.mcp_servers.as_slice() {
                    [existing, v2::McpServer::Acp(server)]
                        if existing == &expected_existing_mcp_server =>
                    {
                        server
                    }
                    servers => {
                        panic!("expected the existing declaration followed by ACP, got {servers:?}")
                    }
                };
                assert_eq!(server.name, "v2-echo");
                let server_id = server.server_id.clone();
                server_id_tx
                    .unbounded_send(server_id.clone())
                    .map_err(Error::into_internal_error)?;
                let round_trip_connection = connection.clone();
                let first_round_trip_tx = first_round_trip_tx.clone();
                let agent_response = agent_response.clone();
                connection.spawn(async move {
                    match run_mcp_round_trip(&round_trip_connection, &server_id, 1).await {
                        Ok(round_trip) => {
                            first_round_trip_tx
                                .unbounded_send(Ok(round_trip))
                                .map_err(Error::into_internal_error)?;
                            responder.respond(agent_response)
                        }
                        Err(error) => {
                            first_round_trip_tx
                                .unbounded_send(Err(error.clone()))
                                .map_err(Error::into_internal_error)?;
                            responder.respond_with_error(error)
                        }
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .with_spawned(move |connection: V2ConnectionTo<Client>| async move {
            let server_id = server_id_rx.next().await.ok_or_else(|| {
                Error::internal_error().data("session/resume did not advertise an MCP server")
            })?;
            let mut sequence = 1;

            while round_trip_trigger_rx.next().await.is_some() {
                sequence += 1;
                let result = run_mcp_round_trip(&connection, &server_id, sequence).await;
                let failed = result.is_err();
                round_trip_tx
                    .unbounded_send(result)
                    .map_err(Error::into_internal_error)?;
                if failed {
                    break;
                }
            }
            Ok(())
        });

    let test = async move {
        let (context_tx, mut context_rx) = mpsc::unbounded();
        let (notice_tx, mut notice_rx) = mpsc::unbounded();
        let (connector_dropped_tx, connector_dropped_rx) = oneshot::channel();
        let (runner_started_tx, runner_started_rx) = oneshot::channel();
        let (runner_dropped_tx, runner_dropped_rx) = oneshot::channel();
        let runner_started = Arc::new(AtomicBool::new(false));

        Client
            .v2()
            .connect_with(agent, async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        implementation(),
                    ))
                    .block_task()
                    .await?;

                let mcp_server = McpServer::<Agent, _>::new(
                    EchoMcpConnect {
                        context_tx,
                        notice_tx,
                        runner_started: runner_started.clone(),
                        dropped_tx: Mutex::new(Some(connector_dropped_tx)),
                    },
                    ProbeRunner {
                        started: runner_started.clone(),
                        started_tx: Some(runner_started_tx),
                        dropped_tx: Some(runner_dropped_tx),
                    },
                );
                let pending_session = connection
                    .resume_session_from(
                        v2::ResumeSessionRequest::new(session_id.clone(), resume_cwd)
                            .additional_directories([additional_directory])
                            .mcp_servers(vec![existing_mcp_server])
                            .replay_from(v2::ReplayFrom::Start(
                                v2::ReplayFromStart::new().meta(replay_meta),
                            ))
                            .meta(setup_meta),
                    )
                    .with_mcp_server(mcp_server)?
                    .start_session();

                runner_started_rx
                    .await
                    .map_err(Error::into_internal_error)?;
                assert!(
                    runner_started.load(Ordering::Acquire),
                    "the MCP runner must be first-polled before session/resume is published"
                );

                assert_round_trip(1, &mut round_trip_rx, &mut context_rx, &mut notice_rx).await?;

                let opened = pending_session.block_task().await?;
                assert_eq!(opened.session().session_id(), &session_id);
                assert_eq!(opened.response(), &expected_response);
                let session = opened.into_session();
                let remaining_session = session.clone();
                drop(session);
                drop(remaining_session);

                round_trip_trigger_tx
                    .unbounded_send(())
                    .map_err(Error::into_internal_error)?;
                assert_round_trip(2, &mut round_trip_rx, &mut context_rx, &mut notice_rx).await?;

                Ok(())
            })
            .await?;

        connector_dropped_rx
            .await
            .map_err(Error::into_internal_error)?;
        runner_dropped_rx.await.map_err(Error::into_internal_error)
    };

    tokio::time::timeout(TIMEOUT, test)
        .await
        .expect("v2 resume MCP attachment test timed out")
}

struct DropTrackedMcpConnect {
    dropped_tx: Mutex<Option<oneshot::Sender<()>>>,
}

impl Drop for DropTrackedMcpConnect {
    fn drop(&mut self) {
        if let Some(dropped_tx) = self
            .dropped_tx
            .get_mut()
            .expect("MCP connector drop mutex poisoned")
            .take()
        {
            let _ = dropped_tx.send(());
        }
    }
}

impl McpServerConnect<Agent> for DropTrackedMcpConnect {
    fn name(&self) -> String {
        "rejected-v2-mcp".to_owned()
    }

    fn connect(&self, _context: McpConnectionTo<Agent>) -> DynConnectTo<role::mcp::Client> {
        panic!("a rejected session must not connect to its MCP server")
    }
}

struct ImmediateErrorRunner;

impl RunWithConnectionTo<Agent> for ImmediateErrorRunner {
    async fn run_with_connection_to(self, _connection: ConnectionTo<Agent>) -> Result<(), Error> {
        Err(Error::internal_error().data("runner failed before publication"))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn immediate_v2_mcp_runner_failure_does_not_publish_or_close_connection() -> Result<(), Error>
{
    let session_new_seen = Arc::new(AtomicBool::new(false));
    let agent_session_new_seen = session_new_seen.clone();
    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(initialize_response(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: v2::NewSessionRequest,
                        responder: Responder<v2::NewSessionResponse>,
                        _connection: V2ConnectionTo<Client>| {
                agent_session_new_seen.store(true, Ordering::Release);
                responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                    "unexpected-session",
                )))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: ConnectionProbeRequest,
                   responder: Responder<ConnectionProbeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(ConnectionProbeResponse {
                    nonce: request.nonce,
                })
            },
            agent_client_protocol::on_receive_request!(),
        );

    let test = async move {
        let (connector_dropped_tx, connector_dropped_rx) = oneshot::channel();

        Client
            .v2()
            .connect_with(agent, async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        implementation(),
                    ))
                    .block_task()
                    .await?;

                let error = connection
                    .build_session(cwd()?)
                    .with_mcp_server(McpServer::<Agent, _>::new(
                        DropTrackedMcpConnect {
                            dropped_tx: Mutex::new(Some(connector_dropped_tx)),
                        },
                        ImmediateErrorRunner,
                    ))?
                    .start_session()
                    .block_task()
                    .await
                    .expect_err("the runner should reject setup before publication");
                assert_eq!(error.code, ErrorCode::InternalError);
                assert_eq!(error.data, Some(json!("runner failed before publication")));

                connector_dropped_rx
                    .await
                    .map_err(Error::into_internal_error)?;

                let nonce = "connection-remains-open".to_owned();
                let response = connection
                    .send_request(ConnectionProbeRequest {
                        nonce: nonce.clone(),
                    })
                    .block_task()
                    .await?;
                assert_eq!(response.nonce, nonce);
                Ok(())
            })
            .await?;

        assert!(
            !session_new_seen.load(Ordering::Acquire),
            "session/new must not be published when runner startup fails"
        );
        Ok(())
    };

    tokio::time::timeout(TIMEOUT, test)
        .await
        .expect("immediate v2 MCP runner failure test timed out")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn immediate_v2_resume_mcp_runner_failure_does_not_publish_or_close_connection()
-> Result<(), Error> {
    let session_resume_seen = Arc::new(AtomicBool::new(false));
    let agent_session_resume_seen = session_resume_seen.clone();
    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(initialize_response(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: v2::ResumeSessionRequest,
                        responder: Responder<v2::ResumeSessionResponse>,
                        _connection: V2ConnectionTo<Client>| {
                agent_session_resume_seen.store(true, Ordering::Release);
                responder.respond(v2::ResumeSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: ConnectionProbeRequest,
                   responder: Responder<ConnectionProbeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(ConnectionProbeResponse {
                    nonce: request.nonce,
                })
            },
            agent_client_protocol::on_receive_request!(),
        );

    let test = async move {
        let (connector_dropped_tx, connector_dropped_rx) = oneshot::channel();

        Client
            .v2()
            .connect_with(agent, async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        implementation(),
                    ))
                    .block_task()
                    .await?;

                let error = connection
                    .resume_session(v2::SessionId::new("resume-not-published"), cwd()?)
                    .with_mcp_server(McpServer::<Agent, _>::new(
                        DropTrackedMcpConnect {
                            dropped_tx: Mutex::new(Some(connector_dropped_tx)),
                        },
                        ImmediateErrorRunner,
                    ))?
                    .start_session()
                    .block_task()
                    .await
                    .expect_err("the runner should reject resume before publication");
                assert_eq!(error.code, ErrorCode::InternalError);
                assert_eq!(error.data, Some(json!("runner failed before publication")));

                connector_dropped_rx
                    .await
                    .map_err(Error::into_internal_error)?;

                let nonce = "connection-remains-open-after-resume".to_owned();
                let response = connection
                    .send_request(ConnectionProbeRequest {
                        nonce: nonce.clone(),
                    })
                    .block_task()
                    .await?;
                assert_eq!(response.nonce, nonce);
                Ok(())
            })
            .await?;

        assert!(
            !session_resume_seen.load(Ordering::Acquire),
            "session/resume must not be published when runner startup fails"
        );
        Ok(())
    };

    tokio::time::timeout(TIMEOUT, test)
        .await
        .expect("immediate v2 resume MCP runner failure test timed out")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_v2_session_stops_prestarted_mcp_attachment() -> Result<(), Error> {
    let runner_started = Arc::new(AtomicBool::new(false));
    let runner_started_before_request = runner_started.clone();
    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(initialize_response(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::NewSessionRequest,
                        responder: Responder<v2::NewSessionResponse>,
                        _connection: V2ConnectionTo<Client>| {
                assert!(matches!(
                    request.mcp_servers.as_slice(),
                    [v2::McpServer::Acp(server)] if server.name == "rejected-v2-mcp"
                ));
                assert!(
                    runner_started_before_request.load(Ordering::Acquire),
                    "the MCP runner must be first-polled before session/new is published"
                );
                responder
                    .respond_with_error(Error::invalid_params().data("rejecting v2 MCP session"))
            },
            agent_client_protocol::on_receive_request!(),
        );

    let test = async move {
        let (connector_dropped_tx, connector_dropped_rx) = oneshot::channel();
        let (runner_started_tx, runner_started_rx) = oneshot::channel();
        let (runner_dropped_tx, runner_dropped_rx) = oneshot::channel();

        Client
            .v2()
            .connect_with(agent, async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        implementation(),
                    ))
                    .block_task()
                    .await?;

                let pending_session = connection
                    .build_session(cwd()?)
                    .with_mcp_server(McpServer::<Agent, _>::new(
                        DropTrackedMcpConnect {
                            dropped_tx: Mutex::new(Some(connector_dropped_tx)),
                        },
                        ProbeRunner {
                            started: runner_started.clone(),
                            started_tx: Some(runner_started_tx),
                            dropped_tx: Some(runner_dropped_tx),
                        },
                    ))?
                    .start_session();

                runner_started_rx
                    .await
                    .map_err(Error::into_internal_error)?;
                assert!(
                    runner_started.load(Ordering::Acquire),
                    "a rejected session's MCP runner must start before session/new"
                );

                let error = pending_session
                    .block_task()
                    .await
                    .expect_err("the agent should reject session/new");
                assert_eq!(error.code, ErrorCode::InvalidParams);

                runner_dropped_rx
                    .await
                    .map_err(Error::into_internal_error)?;
                assert!(
                    runner_started.load(Ordering::Acquire),
                    "the rejected session's MCP runner must have been first-polled"
                );
                connector_dropped_rx
                    .await
                    .map_err(Error::into_internal_error)?;
                Ok(())
            })
            .await
    };

    tokio::time::timeout(TIMEOUT, test)
        .await
        .expect("rejected v2 MCP session cleanup test timed out")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_v2_resume_stops_prestarted_mcp_attachment() -> Result<(), Error> {
    let runner_started = Arc::new(AtomicBool::new(false));
    let runner_started_before_request = runner_started.clone();
    let expected_session_id = v2::SessionId::new("rejected-v2-resume");
    let agent_expected_session_id = expected_session_id.clone();
    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(initialize_response(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::ResumeSessionRequest,
                        responder: Responder<v2::ResumeSessionResponse>,
                        _connection: V2ConnectionTo<Client>| {
                assert_eq!(request.session_id, agent_expected_session_id);
                assert!(matches!(
                    request.mcp_servers.as_slice(),
                    [v2::McpServer::Acp(server)] if server.name == "rejected-v2-mcp"
                ));
                assert!(
                    runner_started_before_request.load(Ordering::Acquire),
                    "the MCP runner must be first-polled before session/resume is published"
                );
                responder
                    .respond_with_error(Error::invalid_params().data("rejecting v2 MCP resume"))
            },
            agent_client_protocol::on_receive_request!(),
        );

    let test = async move {
        let (connector_dropped_tx, connector_dropped_rx) = oneshot::channel();
        let (runner_started_tx, runner_started_rx) = oneshot::channel();
        let (runner_dropped_tx, runner_dropped_rx) = oneshot::channel();

        Client
            .v2()
            .connect_with(agent, async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        implementation(),
                    ))
                    .block_task()
                    .await?;

                let pending_session = connection
                    .resume_session(expected_session_id, cwd()?)
                    .with_mcp_server(McpServer::<Agent, _>::new(
                        DropTrackedMcpConnect {
                            dropped_tx: Mutex::new(Some(connector_dropped_tx)),
                        },
                        ProbeRunner {
                            started: runner_started.clone(),
                            started_tx: Some(runner_started_tx),
                            dropped_tx: Some(runner_dropped_tx),
                        },
                    ))?
                    .start_session();

                runner_started_rx
                    .await
                    .map_err(Error::into_internal_error)?;
                assert!(
                    runner_started.load(Ordering::Acquire),
                    "a rejected resume's MCP runner must start before session/resume"
                );

                let error = pending_session
                    .block_task()
                    .await
                    .expect_err("the agent should reject session/resume");
                assert_eq!(error.code, ErrorCode::InvalidParams);

                runner_dropped_rx
                    .await
                    .map_err(Error::into_internal_error)?;
                assert!(
                    runner_started.load(Ordering::Acquire),
                    "the rejected resume's MCP runner must have been first-polled"
                );
                connector_dropped_rx
                    .await
                    .map_err(Error::into_internal_error)?;
                Ok(())
            })
            .await
    };

    tokio::time::timeout(TIMEOUT, test)
        .await
        .expect("rejected v2 resume MCP cleanup test timed out")
}
