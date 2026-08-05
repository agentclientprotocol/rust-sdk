#![cfg(feature = "unstable_protocol_v2")]

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use agent_client_protocol::{
    Agent, ByteStreams, Client, Conductor, ConnectTo, DynConnectTo, Error, NullRun, Proxy,
    Responder, V2ConnectionTo,
    mcp_server::{McpConnectionTo, McpServer, McpServerConnect},
    role,
    schema::{ProtocolVersion, v2},
};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use futures::{StreamExt as _, channel::mpsc};
use serde_json::json;
use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

fn implementation(name: &str) -> v2::Implementation {
    v2::Implementation::new(name, env!("CARGO_PKG_VERSION"))
}

fn meta() -> v2::Meta {
    v2::Meta::from_iter([("preserved".to_owned(), json!({ "nested": true }))])
}

fn existing_server() -> v2::McpServer {
    v2::McpServer::Other(v2::OtherMcpServer::new(
        "_future_transport",
        BTreeMap::from([("futureOption".to_owned(), json!({ "nested": true }))]),
    ))
}

#[derive(Debug, PartialEq, Eq)]
struct ObservedMcpContext {
    server_id: String,
    connection_id: String,
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
            connection_id: context
                .connection_id()
                .expect("an attached MCP connection should have an ID")
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
                            responder: Responder<v2::NewSessionResponse>,
                            connection: V2ConnectionTo<Conductor>| {
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
                            responder: Responder<v2::ResumeSessionResponse>,
                            connection: V2ConnectionTo<Conductor>| {
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
                       responder: Responder<v2::InitializeResponse>,
                       _connection: V2ConnectionTo<Client>| {
                    responder.respond(v2::InitializeResponse::new(
                        request.protocol_version,
                        implementation("recording-v2-agent"),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: v2::NewSessionRequest,
                            responder: Responder<v2::NewSessionResponse>,
                            connection: V2ConnectionTo<Client>| {
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
                            let connected = mcp_connection
                                .send_request(v2::ConnectMcpRequest::new(server_id))
                                .block_task()
                                .await?;
                            mcp_connection
                                .send_request(v2::DisconnectMcpRequest::new(
                                    connected.connection_id,
                                ))
                                .block_task()
                                .await?;
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
                            responder: Responder<v2::ResumeSessionResponse>,
                            _connection: V2ConnectionTo<Client>| {
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
    editor_task: impl AsyncFnOnce(V2ConnectionTo<Agent>) -> Result<(), Error>,
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
            .expect("global MCP connect/disconnect round trip should not hang")
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
    assert!(
        !mcp_contexts[0].connection_id.is_empty(),
        "the global MCP connection should receive a connection ID"
    );
    Ok(())
}
