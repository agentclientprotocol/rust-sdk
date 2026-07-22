//! Integration tests for the public MCP-over-ACP compatibility proxy.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, ConnectMcpRequest, ConnectMcpResponse, InitializeRequest,
    InitializeResponse, LoadSessionRequest, LoadSessionResponse, McpCapabilities, McpServer,
    McpServerAcp, NewSessionRequest, NewSessionResponse, ResumeSessionRequest,
    ResumeSessionResponse, SessionCapabilities, SessionResumeCapabilities,
};
use agent_client_protocol::{Agent, Client, Conductor, ConnectTo, Proxy};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use agent_client_protocol_polyfill::mcp_over_acp::McpOverAcpPolyfill;
use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const SERVER_NAME: &str = "shared-server";
const SERVER_ID: &str = "shared-server-id";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetupMethod {
    New,
    Load,
    Resume,
}

#[derive(Debug)]
struct SetupRequest {
    method: SetupMethod,
    mcp_servers: Vec<McpServer>,
}

#[derive(Default)]
struct ObservedRequests {
    setup: Mutex<Vec<SetupRequest>>,
}

impl ObservedRequests {
    fn record(&self, method: SetupMethod, mcp_servers: Vec<McpServer>) {
        self.setup
            .lock()
            .expect("setup request mutex should not be poisoned")
            .push(SetupRequest {
                method,
                mcp_servers,
            });
    }
}

struct RecordingAgent {
    capabilities: AgentCapabilities,
    observed: Arc<ObservedRequests>,
}

struct NativeMcpProvider {
    connect_count: Arc<AtomicUsize>,
}

impl ConnectTo<Conductor> for NativeMcpProvider {
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol::Error> {
        Proxy
            .builder()
            .name("native-mcp-provider")
            .on_receive_request_from(
                Agent,
                async move |request: ConnectMcpRequest, responder, _cx| {
                    assert_eq!(request.server_id.to_string(), SERVER_ID);
                    self.connect_count.fetch_add(1, Ordering::SeqCst);
                    responder.respond(ConnectMcpResponse::new("test-connection-id"))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}

impl ConnectTo<Client> for RecordingAgent {
    async fn connect_to(
        self,
        client: impl ConnectTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error> {
        let capabilities = self.capabilities;
        let new_observed = self.observed.clone();
        let load_observed = self.observed.clone();
        let resume_observed = self.observed;

        Agent
            .builder()
            .name("recording-agent")
            .on_receive_request(
                async move |request: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(request.protocol_version)
                            .agent_capabilities(capabilities.clone()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: NewSessionRequest, responder, _cx| {
                    new_observed.record(SetupMethod::New, request.mcp_servers);
                    responder.respond(NewSessionResponse::new("session-id"))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: LoadSessionRequest, responder, _cx| {
                    load_observed.record(SetupMethod::Load, request.mcp_servers);
                    responder.respond(LoadSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: ResumeSessionRequest, responder, _cx| {
                    resume_observed.record(SetupMethod::Resume, request.mcp_servers);
                    responder.respond(ResumeSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}

fn agent_capabilities(mcp_capabilities: McpCapabilities) -> AgentCapabilities {
    AgentCapabilities::new()
        .load_session(true)
        .session_capabilities(SessionCapabilities::new().resume(SessionResumeCapabilities::new()))
        .mcp_capabilities(mcp_capabilities)
}

fn native_server() -> McpServer {
    let meta = serde_json::Map::from_iter([(
        "source".to_string(),
        serde_json::Value::String("integration-test".to_string()),
    )]);
    McpServer::Acp(McpServerAcp::new(SERVER_NAME, SERVER_ID).meta(meta))
}

async fn recv<T: agent_client_protocol::JsonRpcResponse + Send>(
    response: agent_client_protocol::SentRequest<T>,
) -> Result<T, agent_client_protocol::Error> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    response.on_receiving_result(async move |result| {
        tx.send(result)
            .map_err(|_| agent_client_protocol::Error::internal_error())
    })?;
    rx.await
        .map_err(|_| agent_client_protocol::Error::internal_error())?
}

async fn run_with_polyfill(
    agent: RecordingAgent,
    provider_connect_count: Arc<AtomicUsize>,
    editor_task: impl AsyncFnOnce(
        agent_client_protocol::ConnectionTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error>,
) -> Result<(), agent_client_protocol::Error> {
    drop(
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init(),
    );

    let (editor_out, conductor_in) = duplex(4096);
    let (conductor_out, editor_in) = duplex(4096);

    let transport =
        agent_client_protocol::ByteStreams::new(editor_out.compat_write(), editor_in.compat());

    Client
        .builder()
        .name("polyfill-test-client")
        .with_spawned(|_cx| async move {
            ConductorImpl::new_agent(
                "polyfill-test-conductor".to_string(),
                ProxiesAndAgent::new(agent)
                    .proxy(NativeMcpProvider {
                        connect_count: provider_connect_count,
                    })
                    .proxy(McpOverAcpPolyfill::http()),
            )
            .run(agent_client_protocol::ByteStreams::new(
                conductor_out.compat_write(),
                conductor_in.compat(),
            ))
            .await
        })
        .connect_with(transport, editor_task)
        .await
}

#[tokio::test]
async fn http_downstream_receives_stable_transformed_declarations_for_all_setup_methods()
-> Result<(), agent_client_protocol::Error> {
    let observed = Arc::new(ObservedRequests::default());
    let agent = RecordingAgent {
        capabilities: agent_capabilities(McpCapabilities::new().http(true)),
        observed: observed.clone(),
    };
    let connect_count = Arc::new(AtomicUsize::new(0));

    run_with_polyfill(agent, connect_count.clone(), async |connection| {
        let initialize =
            recv(connection.send_request(InitializeRequest::new(ProtocolVersion::V1))).await?;
        assert!(initialize.agent_capabilities.mcp_capabilities.http);
        assert!(
            initialize.agent_capabilities.mcp_capabilities.acp,
            "the HTTP adapter should advertise native MCP support upstream"
        );

        let cwd = PathBuf::from("/tmp");
        let session =
            recv(connection.send_request(
                NewSessionRequest::new(cwd.clone()).mcp_servers(vec![native_server()]),
            ))
            .await?;
        recv(
            connection.send_request(
                LoadSessionRequest::new(session.session_id.clone(), cwd.clone())
                    .mcp_servers(vec![native_server()]),
            ),
        )
        .await?;
        recv(connection.send_request(
            ResumeSessionRequest::new(session.session_id, cwd).mcp_servers(vec![native_server()]),
        ))
        .await?;

        Ok(())
    })
    .await?;

    let setup = observed
        .setup
        .lock()
        .expect("setup request mutex should not be poisoned");
    assert_eq!(
        connect_count.load(Ordering::SeqCst),
        1,
        "one reused listener should create one native MCP connection"
    );
    assert_eq!(setup.len(), 3);
    assert_eq!(setup[0].method, SetupMethod::New);
    assert_eq!(setup[1].method, SetupMethod::Load);
    assert_eq!(setup[2].method, SetupMethod::Resume);

    let expected_meta = serde_json::Map::from_iter([(
        "source".to_string(),
        serde_json::Value::String("integration-test".to_string()),
    )]);
    let mut endpoint = None;
    for request in setup.iter() {
        let [McpServer::Http(server)] = request.mcp_servers.as_slice() else {
            panic!(
                "expected one HTTP MCP declaration for {:?}, got {:?}",
                request.method, request.mcp_servers
            );
        };
        assert_eq!(server.name, SERVER_NAME);
        assert_eq!(server.meta.as_ref(), Some(&expected_meta));
        assert!(server.headers.is_empty());
        assert!(server.url.starts_with("http://127.0.0.1:"));
        if let Some(endpoint) = &endpoint {
            assert_eq!(
                &server.url, endpoint,
                "the same ACP server ID should reuse one listener"
            );
        } else {
            endpoint = Some(server.url.clone());
        }
    }

    Ok(())
}

#[tokio::test]
async fn native_downstream_keeps_capability_and_declaration_unchanged()
-> Result<(), agent_client_protocol::Error> {
    let observed = Arc::new(ObservedRequests::default());
    let agent = RecordingAgent {
        capabilities: agent_capabilities(McpCapabilities::new().acp(true)),
        observed: observed.clone(),
    };
    let declaration = native_server();
    let expected = declaration.clone();
    let connect_count = Arc::new(AtomicUsize::new(0));

    run_with_polyfill(agent, connect_count.clone(), async move |connection| {
        let initialize =
            recv(connection.send_request(InitializeRequest::new(ProtocolVersion::V1))).await?;
        assert!(!initialize.agent_capabilities.mcp_capabilities.http);
        assert!(initialize.agent_capabilities.mcp_capabilities.acp);

        recv(connection.send_request(
            NewSessionRequest::new(PathBuf::from("/tmp")).mcp_servers(vec![declaration]),
        ))
        .await?;
        Ok(())
    })
    .await?;

    let setup = observed
        .setup
        .lock()
        .expect("setup request mutex should not be poisoned");
    assert_eq!(setup.len(), 1);
    assert_eq!(setup[0].mcp_servers, vec![expected]);
    assert_eq!(
        connect_count.load(Ordering::SeqCst),
        0,
        "a native-capable downstream should not be routed through the HTTP adapter"
    );

    Ok(())
}

#[tokio::test]
async fn unsupported_downstream_does_not_gain_native_capability()
-> Result<(), agent_client_protocol::Error> {
    let agent = RecordingAgent {
        capabilities: agent_capabilities(McpCapabilities::new()),
        observed: Arc::default(),
    };

    run_with_polyfill(agent, Arc::default(), async |connection| {
        let initialize =
            recv(connection.send_request(InitializeRequest::new(ProtocolVersion::V1))).await?;
        assert!(!initialize.agent_capabilities.mcp_capabilities.http);
        assert!(
            !initialize.agent_capabilities.mcp_capabilities.acp,
            "the adapter must not advertise native MCP without a usable downstream transport"
        );

        let error = recv(connection.send_request(
            NewSessionRequest::new(PathBuf::from("/tmp")).mcp_servers(vec![native_server()]),
        ))
        .await
        .expect_err("native declarations must not reach an unsupported downstream agent");
        assert_eq!(error.code, agent_client_protocol::ErrorCode::InvalidParams);
        assert_eq!(
            error.data,
            Some(serde_json::json!(
                "the downstream agent supports neither native nor HTTP MCP transport"
            ))
        );
        Ok(())
    })
    .await
}
