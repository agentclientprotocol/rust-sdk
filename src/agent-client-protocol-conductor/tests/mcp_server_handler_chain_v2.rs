//! Checks that attaching an MCP server preserves the v2 handler chain and rejects duplicate server names.

#![cfg(feature = "unstable_protocol_v2")]

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use agent_client_protocol::v2::ConnectionTo;
use agent_client_protocol::{
    Agent, ByteStreams, Client, Conductor, ConnectTo, DynConnectTo, Error, JsonRpcRequest,
    JsonRpcResponse, NullRun, Proxy, Responder,
    mcp_server::{
        McpConnectionTo, McpOutcome, McpRequest, McpRequestContext, McpServer, McpServerConnect,
        McpService,
    },
    role,
    schema::ProtocolVersion,
    v2::schema as v2,
};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use futures::{StreamExt as _, channel::mpsc};
use serde_json::json;
use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcRequest)]
#[request(method = "_test/probe", response = ProbeResponse)]
struct ProbeRequest {}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcResponse)]
struct ProbeResponse {
    #[serde(rename = "resultType")]
    result_type: String,
}

fn implementation(name: &str) -> v2::Implementation {
    v2::Implementation::new(name, env!("CARGO_PKG_VERSION"))
}

fn meta() -> v2::Meta {
    v2::Meta::from_iter([("preserved".to_owned(), json!({ "nested": true }))])
}

fn existing_server() -> v2::McpServer {
    v2::McpServer::Other(v2::OtherMcpServer::new(
        "_future_transport",
        [("futureOption".to_owned(), json!({ "nested": true }))].into(),
    ))
}

#[derive(Debug, PartialEq, Eq)]
struct ObservedMcpContext {
    server_id: String,
    request_id: String,
}

struct RecordingMcpConnect {
    contexts: Arc<Mutex<Vec<ObservedMcpContext>>>,
}

impl McpServerConnect<Conductor> for RecordingMcpConnect {
    fn name(&self) -> String {
        "global-v2-server".to_owned()
    }

    fn connect(&self, context: McpConnectionTo<Conductor>) -> DynConnectTo<role::mcp::Client> {
        self.contexts.lock().unwrap().push(ObservedMcpContext {
            server_id: context
                .server_id()
                .expect("the global MCP server should be attached through ACP")
                .to_string(),
            request_id: context
                .request_id()
                .expect("an attached MCP request should have an ID")
                .to_string(),
        });
        DynConnectTo::new(PendingMcpComponent)
    }
}

struct PendingMcpComponent;

impl ConnectTo<role::mcp::Client> for PendingMcpComponent {
    async fn connect_to(self, client: impl ConnectTo<role::mcp::Server>) -> Result<(), Error> {
        role::mcp::Server
            .builder()
            .on_receive_request(
                async |_request: ProbeRequest, responder: Responder<ProbeResponse>, _connection| {
                    responder.respond(ProbeResponse {
                        result_type: "complete".to_owned(),
                    })
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(client, async |_connection| {
                std::future::pending::<Result<(), Error>>().await
            })
            .await
    }
}

struct GlobalMcpProxy {
    setup_handler_calls: Arc<AtomicUsize>,
    mcp_contexts: Arc<Mutex<Vec<ObservedMcpContext>>>,
}

impl ConnectTo<Conductor> for GlobalMcpProxy {
    async fn connect_to(self, conductor: impl ConnectTo<Proxy>) -> Result<(), Error> {
        let new_calls = self.setup_handler_calls.clone();
        let resume_calls = self.setup_handler_calls;
        let mcp_server = McpServer::new(
            RecordingMcpConnect {
                contexts: self.mcp_contexts,
            },
            NullRun,
        );

        Proxy
            .v2()
            .name("global-v2-mcp-proxy")
            .with_mcp_server(mcp_server)
            .on_receive_request_from(
                Client,
                async move |request: v2::NewSessionRequest,
                            responder: Responder<
                    v2::NewSessionResponse,
                    agent_client_protocol::V2,
                >,
                            connection: ConnectionTo<Conductor>| {
                    new_calls.fetch_add(1, Ordering::SeqCst);
                    connection
                        .send_request_to(Agent, request)
                        .forward_response_to(responder)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request_from(
                Client,
                async move |request: v2::ResumeSessionRequest,
                            responder: Responder<
                    v2::ResumeSessionResponse,
                    agent_client_protocol::V2,
                >,
                            connection: ConnectionTo<Conductor>| {
                    resume_calls.fetch_add(1, Ordering::SeqCst);
                    connection
                        .send_request_to(Agent, request)
                        .forward_response_to(responder)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(conductor)
            .await
    }
}

#[derive(Default)]
struct ObservedSetup {
    server_ids: Mutex<Vec<v2::McpServerAcpId>>,
}

impl ObservedSetup {
    fn record(
        &self,
        servers: &[v2::McpServer],
        expected_existing: &v2::McpServer,
    ) -> Result<v2::McpServerAcpId, Error> {
        let server = match servers {
            [existing, v2::McpServer::Acp(server)] if existing == expected_existing => server,
            servers => {
                return Err(Error::internal_error()
                    .data(format!("unexpected MCP declarations: {servers:?}")));
            }
        };
        if server.name != "global-v2-server" {
            return Err(Error::internal_error().data(format!(
                "unexpected global MCP server name: {}",
                server.name
            )));
        }
        self.server_ids
            .lock()
            .unwrap()
            .push(server.server_id.clone());
        Ok(server.server_id.clone())
    }
}

struct RecordingAgent {
    setup: Arc<ObservedSetup>,
    expected_existing: v2::McpServer,
    cwd: PathBuf,
    additional_directory: PathBuf,
    round_trip_tx: mpsc::UnboundedSender<Result<(), Error>>,
}

impl ConnectTo<Client> for RecordingAgent {
    async fn connect_to(self, client: impl ConnectTo<Agent>) -> Result<(), Error> {
        let new_setup = self.setup.clone();
        let resume_setup = self.setup;
        let new_existing = self.expected_existing.clone();
        let resume_existing = self.expected_existing;
        let new_cwd = self.cwd.clone();
        let resume_cwd = self.cwd;
        let new_additional = self.additional_directory.clone();
        let resume_additional = self.additional_directory;
        let round_trip_tx = self.round_trip_tx;

        Agent
            .v2()
            .name("recording-v2-agent")
            .on_receive_request(
                async |request: v2::InitializeRequest,
                       responder: Responder<v2::InitializeResponse, agent_client_protocol::V2>,
                       _connection: ConnectionTo<Client>| {
                    responder.respond(v2::InitializeResponse::new(
                        request.protocol_version,
                        implementation("recording-v2-agent"),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: v2::NewSessionRequest,
                            responder: Responder<
                    v2::NewSessionResponse,
                    agent_client_protocol::V2,
                >,
                            connection: ConnectionTo<Client>| {
                    let new_setup = new_setup.clone();
                    let new_existing = new_existing.clone();
                    let new_cwd = new_cwd.clone();
                    let new_additional = new_additional.clone();
                    let round_trip_tx = round_trip_tx.clone();
                    assert_eq!(request.cwd, v2::AbsolutePath::new(new_cwd));
                    assert_eq!(
                        request.additional_directories.as_slice(),
                        [v2::AbsolutePath::new(new_additional)]
                    );
                    assert_eq!(request.meta.as_ref(), Some(&meta()));
                    let server_id = new_setup.record(&request.mcp_servers, &new_existing)?;
                    let mcp_connection = connection.clone();
                    connection.spawn(async move {
                        let result = async {
                            let response = mcp_connection
                                .send_request(
                                    v2::MessageMcpRequest::new(
                                        server_id,
                                        "global-v2-probe",
                                        "_test/probe",
                                    )
                                    .params(serde_json::Map::from_iter([(
                                        "_meta".to_owned(),
                                        json!({
                                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                                            "io.modelcontextprotocol/clientCapabilities": {}
                                        }),
                                    )])),
                                )
                                .block_task()
                                .await?;
                            let v2::MessageMcpResponse::Result { result, .. } = response else {
                                panic!("global MCP probe returned an inner error")
                            };
                            assert_eq!(result["resultType"], "complete");
                            Ok(())
                        }
                        .await;
                        round_trip_tx
                            .unbounded_send(result)
                            .map_err(Error::into_internal_error)
                    })?;
                    responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                        "global-v2-session",
                    )))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: v2::ResumeSessionRequest,
                            responder: Responder<
                    v2::ResumeSessionResponse,
                    agent_client_protocol::V2,
                >,
                            _connection: ConnectionTo<Client>| {
                    let resume_setup = resume_setup.clone();
                    let resume_existing = resume_existing.clone();
                    let resume_cwd = resume_cwd.clone();
                    let resume_additional = resume_additional.clone();
                    assert_eq!(request.cwd, v2::AbsolutePath::new(resume_cwd));
                    assert_eq!(
                        request.additional_directories.as_slice(),
                        [v2::AbsolutePath::new(resume_additional)]
                    );
                    assert_eq!(
                        request.replay_from,
                        Some(v2::ReplayFrom::Start(
                            v2::ReplayFromStart::new().meta(meta())
                        ))
                    );
                    assert_eq!(request.meta.as_ref(), Some(&meta()));
                    resume_setup.record(&request.mcp_servers, &resume_existing)?;
                    responder.respond(v2::ResumeSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}

async fn run_with_conductor(
    proxy: DynConnectTo<Conductor>,
    agent: DynConnectTo<Client>,
    editor_task: impl AsyncFnOnce(ConnectionTo<Agent>) -> Result<(), Error>,
) -> Result<(), Error> {
    let (editor_out, conductor_in) = duplex(4096);
    let (conductor_out, editor_in) = duplex(4096);
    let transport = ByteStreams::new(editor_out.compat_write(), editor_in.compat());

    Client
        .v2()
        .name("v2-editor")
        .with_spawned(|_connection| async move {
            ConductorImpl::new_agent("v2-conductor", ProxiesAndAgent::new(agent).proxy(proxy))
                .run(ByteStreams::new(
                    conductor_out.compat_write(),
                    conductor_in.compat(),
                ))
                .await
        })
        .connect_with(transport, editor_task)
        .await
}

#[tokio::test]
async fn v2_global_mcp_attachment_preserves_setup_and_continues_handler_chain() -> Result<(), Error>
{
    let setup_handler_calls = Arc::new(AtomicUsize::new(0));
    let observed_setup = Arc::new(ObservedSetup::default());
    let mcp_contexts = Arc::new(Mutex::new(Vec::new()));
    let (round_trip_tx, mut round_trip_rx) = mpsc::unbounded();
    let cwd = PathBuf::from("/tmp/global-v2-mcp");
    let additional_directory = PathBuf::from("/tmp/global-v2-mcp-additional");
    let existing_server = existing_server();

    let proxy = DynConnectTo::new(GlobalMcpProxy {
        setup_handler_calls: setup_handler_calls.clone(),
        mcp_contexts: mcp_contexts.clone(),
    });
    let agent = DynConnectTo::new(RecordingAgent {
        setup: observed_setup.clone(),
        expected_existing: existing_server.clone(),
        cwd: cwd.clone(),
        additional_directory: additional_directory.clone(),
        round_trip_tx,
    });

    run_with_conductor(proxy, agent, async move |connection| {
        connection
            .send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                implementation("v2-editor"),
            ))
            .block_task()
            .await?;

        let new_session = connection
            .send_request(
                v2::NewSessionRequest::new(cwd.clone())
                    .additional_directories([additional_directory.clone()])
                    .mcp_servers(vec![existing_server.clone()])
                    .meta(meta()),
            )
            .block_task()
            .await?;

        tokio::time::timeout(std::time::Duration::from_secs(2), round_trip_rx.next())
            .await
            .expect("global MCP request should not hang")
            .ok_or_else(|| Error::internal_error().data("MCP round-trip channel closed"))??;

        connection
            .send_request(
                v2::ResumeSessionRequest::new(new_session.session_id, cwd)
                    .additional_directories([additional_directory])
                    .mcp_servers(vec![existing_server])
                    .replay_from(v2::ReplayFrom::Start(
                        v2::ReplayFromStart::new().meta(meta()),
                    ))
                    .meta(meta()),
            )
            .block_task()
            .await?;
        Ok(())
    })
    .await?;

    assert_eq!(
        setup_handler_calls.load(Ordering::SeqCst),
        2,
        "both typed handlers after the global MCP handler should run"
    );
    let server_ids = observed_setup.server_ids.lock().unwrap();
    assert_eq!(server_ids.len(), 2);
    assert_eq!(
        server_ids[0], server_ids[1],
        "a global MCP server must advertise one stable server ID"
    );
    let mcp_contexts = mcp_contexts.lock().unwrap();
    assert_eq!(mcp_contexts.len(), 1);
    assert_eq!(mcp_contexts[0].server_id, server_ids[0].to_string());
    assert_eq!(
        mcp_contexts[0].request_id, "global-v2-probe",
        "the global MCP request should retain its logical request ID"
    );
    Ok(())
}

/// Records the `mcpServers` of every setup request that reaches the agent.
struct SetupRecordingAgent {
    received: Arc<Mutex<Vec<Vec<v2::McpServer>>>>,
}

impl ConnectTo<Client> for SetupRecordingAgent {
    async fn connect_to(self, client: impl ConnectTo<Agent>) -> Result<(), Error> {
        let new_received = self.received.clone();
        let resume_received = self.received;

        Agent
            .v2()
            .name("setup-recording-v2-agent")
            .on_receive_request(
                async |request: v2::InitializeRequest,
                       responder: Responder<v2::InitializeResponse, agent_client_protocol::V2>,
                       _connection: ConnectionTo<Client>| {
                    responder.respond(v2::InitializeResponse::new(
                        request.protocol_version,
                        implementation("setup-recording-v2-agent"),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: v2::NewSessionRequest,
                            responder: Responder<
                    v2::NewSessionResponse,
                    agent_client_protocol::V2,
                >,
                            _connection: ConnectionTo<Client>| {
                    new_received.lock().unwrap().push(request.mcp_servers);
                    responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                        "setup-recording-session",
                    )))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: v2::ResumeSessionRequest,
                            responder: Responder<
                    v2::ResumeSessionResponse,
                    agent_client_protocol::V2,
                >,
                            _connection: ConnectionTo<Client>| {
                    resume_received.lock().unwrap().push(request.mcp_servers);
                    responder.respond(v2::ResumeSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}

fn assert_proxy_name_collision<T: std::fmt::Debug>(result: Result<T, Error>) {
    let error = result.expect_err("a colliding MCP server name must be rejected");
    assert_eq!(error.code, agent_client_protocol::ErrorCode::INVALID_PARAMS);
    let message = error
        .data
        .as_ref()
        .and_then(serde_json::Value::as_str)
        .expect("the error should explain the collision");
    assert!(
        message.contains("`global-v2-server`") && message.contains("configured by the proxy"),
        "unexpected error message: {message}"
    );
}

#[tokio::test]
async fn v2_global_mcp_attachment_rejects_client_server_with_same_name() -> Result<(), Error> {
    let setup_handler_calls = Arc::new(AtomicUsize::new(0));
    let received = Arc::new(Mutex::new(Vec::new()));
    let cwd = PathBuf::from("/tmp/global-v2-mcp");
    let colliding_server = v2::McpServer::Http(v2::McpServerHttp::new(
        "global-v2-server",
        "https://example.com/mcp",
    ));
    let client_server = v2::McpServer::Http(v2::McpServerHttp::new(
        "client-server",
        "https://example.com/mcp",
    ));

    let proxy = DynConnectTo::new(GlobalMcpProxy {
        setup_handler_calls: setup_handler_calls.clone(),
        mcp_contexts: Arc::new(Mutex::new(Vec::new())),
    });
    let agent = DynConnectTo::new(SetupRecordingAgent {
        received: received.clone(),
    });

    let forwarded_client_server = client_server.clone();
    run_with_conductor(proxy, agent, async move |connection| {
        connection
            .send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                implementation("v2-editor"),
            ))
            .block_task()
            .await?;

        assert_proxy_name_collision(
            connection
                .send_request(
                    v2::NewSessionRequest::new(cwd.clone())
                        .mcp_servers(vec![client_server.clone(), colliding_server.clone()]),
                )
                .block_task()
                .await,
        );
        assert_proxy_name_collision(
            connection
                .send_request(
                    v2::ResumeSessionRequest::new(
                        v2::SessionId::new("existing-session"),
                        cwd.clone(),
                    )
                    .mcp_servers(vec![colliding_server]),
                )
                .block_task()
                .await,
        );

        connection
            .send_request(v2::NewSessionRequest::new(cwd).mcp_servers(vec![client_server]))
            .block_task()
            .await?;
        Ok(())
    })
    .await?;

    assert_eq!(
        setup_handler_calls.load(Ordering::SeqCst),
        1,
        "only the non-colliding request should reach later proxy handlers"
    );
    let received = received.lock().unwrap();
    match received.as_slice() {
        [servers] => match servers.as_slice() {
            [existing, v2::McpServer::Acp(global)] => {
                assert_eq!(existing, &forwarded_client_server);
                assert_eq!(global.name, "global-v2-server");
            }
            servers => panic!("unexpected forwarded MCP declarations: {servers:?}"),
        },
        received => panic!("only the non-colliding request should be forwarded: {received:?}"),
    }
    Ok(())
}

/// Counts the requests it runs and answers with the session each was sent
/// for.
struct CountingService {
    runs: Arc<AtomicUsize>,
}

impl McpService<Conductor> for CountingService {
    fn execute(
        &self,
        _request: McpRequest,
        context: McpRequestContext<Conductor>,
    ) -> futures::future::BoxFuture<'static, Result<McpOutcome, Error>> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let result = json!({"sessionId": context.session_id()});
        Box::pin(async move { Ok(McpOutcome::Result(result)) })
    }
}

/// A proxy whose globally attached MCP server needs the session of every
/// request.
struct GatedMcpProxy {
    runs: Arc<AtomicUsize>,
}

impl ConnectTo<Conductor> for GatedMcpProxy {
    async fn connect_to(self, conductor: impl ConnectTo<Proxy>) -> Result<(), Error> {
        let service = CountingService { runs: self.runs };
        let mcp_server = McpServer::new_service("gated-v2-server", service, NullRun)
            .with_admission(|request| {
                request.require_session_id()?;
                Ok(())
            });
        Proxy
            .v2()
            .name("gated-v2-mcp-proxy")
            .with_mcp_server(mcp_server)
            .connect_to(conductor)
            .await
    }
}

type McpCallOutcome = Result<v2::MessageMcpResponse, Error>;

/// An agent that calls the injected server twice for each new session:
/// without a session, then for the session `child`.
struct CallingAgent {
    outcomes: mpsc::UnboundedSender<McpCallOutcome>,
}

impl ConnectTo<Client> for CallingAgent {
    async fn connect_to(self, client: impl ConnectTo<Agent>) -> Result<(), Error> {
        let outcomes = self.outcomes;
        Agent
            .v2()
            .name("calling-v2-agent")
            .on_receive_request(
                async |request: v2::InitializeRequest,
                       responder: Responder<v2::InitializeResponse, agent_client_protocol::V2>,
                       _connection: ConnectionTo<Client>| {
                    responder.respond(v2::InitializeResponse::new(
                        request.protocol_version,
                        implementation("calling-v2-agent"),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: v2::NewSessionRequest,
                            responder: Responder<
                    v2::NewSessionResponse,
                    agent_client_protocol::V2,
                >,
                            connection: ConnectionTo<Client>| {
                    let [v2::McpServer::Acp(server)] = request.mcp_servers.as_slice() else {
                        return Err(Error::internal_error().data("expected the injected server"));
                    };
                    let server_id = server.server_id.clone();
                    let outcomes = outcomes.clone();
                    let mcp_connection = connection.clone();
                    connection.spawn(async move {
                        let params = serde_json::Map::from_iter([(
                            "_meta".to_owned(),
                            json!({
                                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                                "io.modelcontextprotocol/clientCapabilities": {}
                            }),
                        )]);
                        for session_id in [None, Some(v2::SessionId::new("child"))] {
                            let request = v2::MessageMcpRequest::new(
                                server_id.clone(),
                                "gated-call",
                                "tools/call",
                            )
                            .params(params.clone())
                            .session_id(session_id);
                            let outcome = mcp_connection.send_request(request).block_task().await;
                            outcomes
                                .unbounded_send(outcome)
                                .map_err(Error::into_internal_error)?;
                        }
                        Ok(())
                    })?;
                    responder.respond(v2::NewSessionResponse::new(v2::SessionId::new("root")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}

/// The admission check of a globally attached server decides before a request
/// runs, like the check of a server attached to one session.
#[tokio::test]
async fn v2_global_mcp_admission_check_rejects_before_the_service_runs() -> Result<(), Error> {
    let runs = Arc::new(AtomicUsize::new(0));
    let (outcomes_tx, mut outcomes) = mpsc::unbounded();
    let proxy = DynConnectTo::new(GatedMcpProxy { runs: runs.clone() });
    let agent = DynConnectTo::new(CallingAgent {
        outcomes: outcomes_tx,
    });

    run_with_conductor(proxy, agent, async move |connection| {
        connection
            .send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                implementation("v2-editor"),
            ))
            .block_task()
            .await?;
        connection
            .send_request(v2::NewSessionRequest::new(PathBuf::from(
                "/tmp/gated-v2-mcp",
            )))
            .block_task()
            .await?;

        let mut next = async || {
            tokio::time::timeout(std::time::Duration::from_secs(5), outcomes.next())
                .await
                .expect("the MCP call should not hang")
                .expect("the agent reports each call")
        };
        let rejected = next()
            .await
            .expect_err("a call without a session is rejected");
        assert_eq!(
            serde_json::to_value(rejected)?,
            json!({
                "code": -32602,
                "message": "Invalid params",
                "data": "this MCP server requires sessionId on mcp/message",
            })
        );

        let v2::MessageMcpResponse::Result { result, .. } = next().await? else {
            panic!("the attributed call should return a result")
        };
        assert_eq!(result, json!({"sessionId": "child"}));
        // Two calls, and the service ran for the admitted one only.
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        Ok(())
    })
    .await
}
