#![cfg(feature = "unstable_protocol_v2")]

//! V2 integration coverage for the public MCP-over-ACP compatibility proxy.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use agent_client_protocol::{
    Agent, Client, Conductor, ConnectTo, Proxy, V2ConnectionTo,
    schema::{ProtocolVersion, v2},
};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use agent_client_protocol_polyfill::mcp_over_acp::McpOverAcpPolyfill;
use rmcp::{
    ServiceExt as _,
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const SERVER_NAME: &str = "shared-v2-server";
const SERVER_ID: &str = "shared-v2-server-id";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetupMethod {
    New,
    Resume,
}

#[derive(Debug)]
struct SetupRequest {
    method: SetupMethod,
    mcp_servers: Vec<v2::McpServer>,
}

#[derive(Default)]
struct ObservedRequests {
    setup: Mutex<Vec<SetupRequest>>,
}

impl ObservedRequests {
    fn record(&self, method: SetupMethod, mcp_servers: Vec<v2::McpServer>) {
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
    capabilities: v2::AgentCapabilities,
    observed: Arc<ObservedRequests>,
}

impl ConnectTo<Client> for RecordingAgent {
    async fn connect_to(
        self,
        client: impl ConnectTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error> {
        let capabilities = self.capabilities;
        let new_observed = Arc::clone(&self.observed);
        let resume_observed = self.observed;

        Agent
            .v2()
            .name("recording-v2-agent")
            .on_receive_request(
                async move |request: v2::InitializeRequest, responder, _cx| {
                    assert_eq!(request.protocol_version, ProtocolVersion::V2);
                    responder.respond(
                        v2::InitializeResponse::new(
                            request.protocol_version,
                            implementation("recording-v2-agent"),
                        )
                        .capabilities(capabilities.clone()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: v2::NewSessionRequest, responder, _cx| {
                    new_observed.record(SetupMethod::New, request.mcp_servers);
                    responder.respond(v2::NewSessionResponse::new("v2-session-id"))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: v2::ResumeSessionRequest, responder, _cx| {
                    resume_observed.record(SetupMethod::Resume, request.mcp_servers);
                    responder.respond(v2::ResumeSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}

struct NativeMcpProvider {
    connect_count: Arc<AtomicUsize>,
    request_methods: Arc<Mutex<Vec<String>>>,
    notification_methods: Arc<Mutex<Vec<String>>>,
    disconnect_count: Arc<AtomicUsize>,
}

impl ConnectTo<Conductor> for NativeMcpProvider {
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol::Error> {
        let request_methods = Arc::clone(&self.request_methods);
        let notification_methods = Arc::clone(&self.notification_methods);
        let disconnect_count = Arc::clone(&self.disconnect_count);

        Proxy
            .builder()
            .name("native-v2-mcp-provider")
            .on_receive_request_from(
                Agent,
                async move |request: v2::ConnectMcpRequest, responder, _cx| {
                    assert_eq!(request.server_id.to_string(), SERVER_ID);
                    self.connect_count.fetch_add(1, Ordering::SeqCst);
                    responder.respond(v2::ConnectMcpResponse::new("v2-test-connection-id"))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request_from(
                Agent,
                async move |request: v2::MessageMcpRequest, responder, _cx| {
                    request_methods
                        .lock()
                        .expect("request method mutex should not be poisoned")
                        .push(request.method.clone());
                    match request.method.as_str() {
                        "initialize" => {
                            let protocol_version = request
                                .params
                                .as_ref()
                                .and_then(|params| params.get("protocolVersion"))
                                .cloned()
                                .unwrap_or_else(|| serde_json::json!("2025-06-18"));
                            responder.respond(serde_json::from_value(serde_json::json!({
                                "protocolVersion": protocol_version,
                                "capabilities": {
                                    "tools": {}
                                },
                                "serverInfo": {
                                    "name": "v2-polyfill-test-mcp-server",
                                    "version": env!("CARGO_PKG_VERSION")
                                }
                            }))?)
                        }
                        "tools/list" => responder
                            .respond(serde_json::from_value(serde_json::json!({ "tools": [] }))?),
                        method => responder.respond_with_error(
                            agent_client_protocol::Error::method_not_found().data(method),
                        ),
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification_from(
                Agent,
                async move |notification: v2::MessageMcpNotification, _cx| {
                    notification_methods
                        .lock()
                        .expect("notification method mutex should not be poisoned")
                        .push(notification.method);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request_from(
                Agent,
                async move |_request: v2::DisconnectMcpRequest, responder, _cx| {
                    disconnect_count.fetch_add(1, Ordering::SeqCst);
                    responder.respond(v2::DisconnectMcpResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}

fn implementation(name: &str) -> v2::Implementation {
    v2::Implementation::new(name, env!("CARGO_PKG_VERSION"))
}

fn agent_capabilities(mcp: v2::McpCapabilities) -> v2::AgentCapabilities {
    v2::AgentCapabilities::new().session(v2::SessionCapabilities::new().mcp(mcp))
}

fn initialize_request() -> v2::InitializeRequest {
    v2::InitializeRequest::new(
        ProtocolVersion::V2,
        implementation("v2-polyfill-test-client"),
    )
}

fn server_meta() -> v2::Meta {
    let mut meta = v2::Meta::new();
    meta.insert(
        "source".to_owned(),
        serde_json::Value::String("v2-integration-test".to_owned()),
    );
    meta
}

fn native_server() -> v2::McpServer {
    v2::McpServer::Acp(v2::McpServerAcp::new(SERVER_NAME, SERVER_ID).meta(server_meta()))
}

fn future_server() -> v2::McpServer {
    v2::McpServer::Other(v2::OtherMcpServer::new(
        "_future_transport",
        BTreeMap::from([
            ("name".to_owned(), serde_json::json!("future-v2-server")),
            (
                "configuration".to_owned(),
                serde_json::json!({ "preserve": true }),
            ),
        ]),
    ))
}

fn test_servers() -> Vec<v2::McpServer> {
    vec![native_server(), future_server()]
}

async fn run_with_polyfill(
    agent: RecordingAgent,
    provider_connect_count: Arc<AtomicUsize>,
    provider_request_methods: Arc<Mutex<Vec<String>>>,
    provider_notification_methods: Arc<Mutex<Vec<String>>>,
    provider_disconnect_count: Arc<AtomicUsize>,
    editor_task: impl AsyncFnOnce(V2ConnectionTo<Agent>) -> Result<(), agent_client_protocol::Error>,
) -> Result<(), agent_client_protocol::Error> {
    let (editor_out, conductor_in) = duplex(4096);
    let (conductor_out, editor_in) = duplex(4096);
    let transport =
        agent_client_protocol::ByteStreams::new(editor_out.compat_write(), editor_in.compat());

    Client
        .v2()
        .name("v2-polyfill-test-client")
        .with_spawned(|_cx| async move {
            ConductorImpl::new_agent(
                "v2-polyfill-test-conductor",
                ProxiesAndAgent::new(agent)
                    .proxy(NativeMcpProvider {
                        connect_count: provider_connect_count,
                        request_methods: provider_request_methods,
                        notification_methods: provider_notification_methods,
                        disconnect_count: provider_disconnect_count,
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

fn negotiated_mcp_capabilities(response: &v2::InitializeResponse) -> &v2::McpCapabilities {
    response
        .capabilities
        .session
        .as_ref()
        .expect("the test agent should advertise session support")
        .mcp
        .as_ref()
        .expect("the test agent should advertise MCP support")
}

#[tokio::test]
async fn http_downstream_adapts_v2_capabilities_and_only_transforms_native_servers()
-> Result<(), agent_client_protocol::Error> {
    let observed = Arc::new(ObservedRequests::default());
    let agent = RecordingAgent {
        capabilities: agent_capabilities(
            v2::McpCapabilities::new().http(v2::McpHttpCapabilities::new()),
        ),
        observed: Arc::clone(&observed),
    };
    let connect_count = Arc::new(AtomicUsize::new(0));
    let request_methods = Arc::new(Mutex::new(Vec::new()));
    let notification_methods = Arc::new(Mutex::new(Vec::new()));
    let disconnect_count = Arc::new(AtomicUsize::new(0));

    run_with_polyfill(
        agent,
        Arc::clone(&connect_count),
        Arc::clone(&request_methods),
        Arc::clone(&notification_methods),
        Arc::clone(&disconnect_count),
        async |connection| {
            let initialize = connection
                .send_request(initialize_request())
                .block_task()
                .await?;
            let mcp = negotiated_mcp_capabilities(&initialize);
            assert!(mcp.http.is_some());
            assert!(
                mcp.acp.is_some(),
                "the HTTP adapter should advertise v2 native MCP support upstream"
            );

            let cwd = PathBuf::from("/tmp");
            let session = connection
                .send_request(v2::NewSessionRequest::new(cwd.clone()).mcp_servers(test_servers()))
                .block_task()
                .await?;
            connection
                .send_request(
                    v2::ResumeSessionRequest::new(session.session_id, cwd)
                        .mcp_servers(test_servers()),
                )
                .block_task()
                .await?;

            let endpoint = {
                let setup = observed
                    .setup
                    .lock()
                    .expect("setup request mutex should not be poisoned");
                let v2::McpServer::Http(server) = &setup[0].mcp_servers[0] else {
                    panic!("expected the native declaration to be adapted to HTTP")
                };
                server.url.clone()
            };
            let mcp_client = ()
                .serve(StreamableHttpClientTransport::from_config(
                    StreamableHttpClientTransportConfig::with_uri(endpoint),
                ))
                .await
                .map_err(agent_client_protocol::Error::into_internal_error)?;
            let tools = mcp_client
                .list_tools(None)
                .await
                .map_err(agent_client_protocol::Error::into_internal_error)?;
            assert!(tools.tools.is_empty());
            mcp_client
                .cancel()
                .await
                .map_err(agent_client_protocol::Error::into_internal_error)?;
            Ok(())
        },
    )
    .await?;

    let setup = observed
        .setup
        .lock()
        .expect("setup request mutex should not be poisoned");
    assert_eq!(
        connect_count.load(Ordering::SeqCst),
        1,
        "one reused listener should create one v2 native MCP connection"
    );
    assert_eq!(
        *request_methods
            .lock()
            .expect("request method mutex should not be poisoned"),
        ["initialize", "tools/list"]
    );
    assert_eq!(
        *notification_methods
            .lock()
            .expect("notification method mutex should not be poisoned"),
        ["notifications/initialized"]
    );
    assert_eq!(setup.len(), 2);
    assert_eq!(setup[0].method, SetupMethod::New);
    assert_eq!(setup[1].method, SetupMethod::Resume);

    let expected_future_server = future_server();
    let expected_meta = server_meta();
    let mut endpoint = None;
    for request in setup.iter() {
        assert_eq!(request.mcp_servers.len(), 2);
        let v2::McpServer::Http(server) = &request.mcp_servers[0] else {
            panic!(
                "expected the ACP declaration to become HTTP for {:?}, got {:?}",
                request.method, request.mcp_servers
            );
        };
        assert_eq!(server.name, SERVER_NAME);
        assert_eq!(server.meta.as_ref(), Some(&expected_meta));
        assert!(server.headers.is_empty());
        assert!(server.url.starts_with("http://127.0.0.1:"));
        assert_eq!(
            request.mcp_servers[1], expected_future_server,
            "the polyfill must preserve custom v2 MCP transports"
        );
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
async fn native_v2_downstream_keeps_capability_and_declarations_unchanged()
-> Result<(), agent_client_protocol::Error> {
    let observed = Arc::new(ObservedRequests::default());
    let agent = RecordingAgent {
        capabilities: agent_capabilities(
            v2::McpCapabilities::new().acp(v2::McpAcpCapabilities::new()),
        ),
        observed: Arc::clone(&observed),
    };
    let expected = test_servers();
    let connect_count = Arc::new(AtomicUsize::new(0));
    let request_methods = Arc::new(Mutex::new(Vec::new()));
    let notification_methods = Arc::new(Mutex::new(Vec::new()));
    let disconnect_count = Arc::new(AtomicUsize::new(0));

    run_with_polyfill(
        agent,
        Arc::clone(&connect_count),
        Arc::clone(&request_methods),
        Arc::clone(&notification_methods),
        Arc::clone(&disconnect_count),
        async move |connection| {
            let initialize = connection
                .send_request(initialize_request())
                .block_task()
                .await?;
            let mcp = negotiated_mcp_capabilities(&initialize);
            assert!(mcp.http.is_none());
            assert!(mcp.acp.is_some());

            connection
                .send_request(
                    v2::NewSessionRequest::new(PathBuf::from("/tmp")).mcp_servers(expected.clone()),
                )
                .block_task()
                .await?;
            Ok(())
        },
    )
    .await?;

    let setup = observed
        .setup
        .lock()
        .expect("setup request mutex should not be poisoned");
    assert_eq!(setup.len(), 1);
    assert_eq!(setup[0].mcp_servers, test_servers());
    assert_eq!(
        connect_count.load(Ordering::SeqCst),
        0,
        "a native-capable v2 downstream should bypass the HTTP adapter"
    );
    assert!(
        request_methods
            .lock()
            .expect("request method mutex should not be poisoned")
            .is_empty()
    );
    assert!(
        notification_methods
            .lock()
            .expect("notification method mutex should not be poisoned")
            .is_empty()
    );
    assert_eq!(disconnect_count.load(Ordering::SeqCst), 0);

    Ok(())
}

#[tokio::test]
async fn unavailable_v2_downstream_rejects_native_declarations()
-> Result<(), agent_client_protocol::Error> {
    let observed = Arc::new(ObservedRequests::default());
    let agent = RecordingAgent {
        capabilities: agent_capabilities(v2::McpCapabilities::new()),
        observed: Arc::clone(&observed),
    };
    let connect_count = Arc::new(AtomicUsize::new(0));
    let request_methods = Arc::new(Mutex::new(Vec::new()));
    let notification_methods = Arc::new(Mutex::new(Vec::new()));
    let disconnect_count = Arc::new(AtomicUsize::new(0));

    run_with_polyfill(
        agent,
        Arc::clone(&connect_count),
        Arc::clone(&request_methods),
        Arc::clone(&notification_methods),
        Arc::clone(&disconnect_count),
        async move |connection| {
            let initialize = connection
                .send_request(initialize_request())
                .block_task()
                .await?;
            let mcp = negotiated_mcp_capabilities(&initialize);
            assert!(mcp.http.is_none());
            assert!(mcp.acp.is_none());

            let error = connection
                .send_request(
                    v2::NewSessionRequest::new(PathBuf::from("/tmp"))
                        .mcp_servers(vec![native_server()]),
                )
                .block_task()
                .await
                .expect_err("native MCP should require a downstream transport");
            assert_eq!(error.code, agent_client_protocol::ErrorCode::InvalidParams);
            assert_eq!(
                error.data,
                Some(serde_json::json!(
                    "the downstream agent supports neither native nor HTTP MCP transport"
                ))
            );
            Ok(())
        },
    )
    .await?;

    assert!(
        observed
            .setup
            .lock()
            .expect("setup request mutex should not be poisoned")
            .is_empty(),
        "the rejected request must not reach the downstream agent"
    );
    assert_eq!(connect_count.load(Ordering::SeqCst), 0);
    assert!(
        request_methods
            .lock()
            .expect("request method mutex should not be poisoned")
            .is_empty()
    );
    assert!(
        notification_methods
            .lock()
            .expect("notification method mutex should not be poisoned")
            .is_empty()
    );
    assert_eq!(disconnect_count.load(Ordering::SeqCst), 0);

    Ok(())
}
