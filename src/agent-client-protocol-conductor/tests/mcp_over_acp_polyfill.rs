//! Integration tests for the public MCP-over-ACP compatibility proxy.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, ConnectMcpRequest, ConnectMcpResponse, DisconnectMcpRequest,
    DisconnectMcpResponse, InitializeRequest, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, McpCapabilities, McpServer, McpServerAcp, MessageMcpRequest,
    NewSessionRequest, NewSessionResponse, ResumeSessionRequest, ResumeSessionResponse,
    SessionCapabilities, SessionResumeCapabilities,
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
    native_messages: Mutex<Vec<(String, String)>>,
    disconnect_count: AtomicUsize,
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
    observed: Arc<ObservedRequests>,
}

impl ConnectTo<Conductor> for NativeMcpProvider {
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol::Error> {
        let messages = Arc::clone(&self.observed);
        let disconnected = Arc::clone(&self.observed);
        Proxy
            .builder()
            .name("native-mcp-provider")
            .on_receive_request_from(
                Agent,
                async move |request: ConnectMcpRequest, responder, _cx| {
                    assert_eq!(request.server_id.to_string(), SERVER_ID);
                    let index = self.connect_count.fetch_add(1, Ordering::SeqCst);
                    responder.respond(ConnectMcpResponse::new(format!("test-connection-{index}")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request_from(
                Agent,
                async move |request: MessageMcpRequest, responder, _cx| {
                    messages
                        .native_messages
                        .lock()
                        .unwrap()
                        .push((request.connection_id.to_string(), request.method.clone()));
                    let result = match request.method.as_str() {
                        "initialize" => serde_json::json!({
                            "protocolVersion": "2025-06-18",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "v1-test", "version": "1" }
                        }),
                        "tools/list" => serde_json::json!({ "tools": [] }),
                        _ => {
                            return responder.respond_with_error(
                                agent_client_protocol::Error::method_not_found(),
                            );
                        }
                    };
                    responder.respond(serde_json::from_value(result)?)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request_from(
                Agent,
                async move |_request: DisconnectMcpRequest, responder, _cx| {
                    disconnected.disconnect_count.fetch_add(1, Ordering::SeqCst);
                    responder.respond(DisconnectMcpResponse::new())
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
    let observed = Arc::clone(&agent.observed);
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
                        observed,
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
        0,
        "creating and reusing the listener must not open a logical MCP session"
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
async fn v1_http_sessions_open_lazily_and_disconnect_independently()
-> Result<(), agent_client_protocol::Error> {
    let observed = Arc::new(ObservedRequests::default());
    let connects = Arc::new(AtomicUsize::new(0));
    run_with_polyfill(
        RecordingAgent {
            capabilities: agent_capabilities(McpCapabilities::new().http(true)),
            observed: Arc::clone(&observed),
        },
        Arc::clone(&connects),
        async |connection| {
            recv(connection.send_request(InitializeRequest::new(ProtocolVersion::V1))).await?;
            recv(connection.send_request(
                NewSessionRequest::new(PathBuf::from("/tmp")).mcp_servers(vec![native_server()]),
            ))
            .await?;
            let endpoint = {
                let setup = observed.setup.lock().unwrap();
                let McpServer::Http(server) = &setup[0].mcp_servers[0] else {
                    panic!("expected HTTP adaptation");
                };
                server.url.clone()
            };
            assert_eq!(connects.load(Ordering::SeqCst), 0);
            let http = reqwest::Client::new();
            let init = serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": { "name": "v1-client", "version": "1" }
                }
            });
            let (a, b) = tokio::join!(
                http.post(&endpoint).json(&init).send(),
                http.post(&endpoint).json(&init).send(),
            );
            let a = a.unwrap();
            let b = b.unwrap();
            let a_id = a
                .headers()
                .get("mcp-session-id")
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            let b_id = b
                .headers()
                .get("mcp-session-id")
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            assert_ne!(a_id, b_id);
            assert_eq!(connects.load(Ordering::SeqCst), 2);
            assert!(a.text().await.unwrap().contains("\"result\""));
            assert!(b.text().await.unwrap().contains("\"result\""));
            let tool = serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}
            });
            let (a, b) = tokio::join!(
                http.post(&endpoint)
                    .header("mcp-session-id", &a_id)
                    .json(&tool)
                    .send(),
                http.post(&endpoint)
                    .header("mcp-session-id", &b_id)
                    .json(&tool)
                    .send(),
            );
            assert!(a.unwrap().text().await.unwrap().contains("\"tools\":[]"));
            assert!(b.unwrap().text().await.unwrap().contains("\"tools\":[]"));
            assert_eq!(
                http.delete(&endpoint)
                    .header("mcp-session-id", &a_id)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                reqwest::StatusCode::ACCEPTED
            );
            assert_eq!(observed.disconnect_count.load(Ordering::SeqCst), 1);
            assert_eq!(
                http.post(&endpoint)
                    .header("mcp-session-id", &a_id)
                    .json(&tool)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                reqwest::StatusCode::NOT_FOUND
            );
            assert!(
                http.post(&endpoint)
                    .header("mcp-session-id", &b_id)
                    .json(&tool)
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap()
                    .contains("\"tools\":[]")
            );
            let c = http.post(&endpoint).json(&init).send().await.unwrap();
            let c_id = c
                .headers()
                .get("mcp-session-id")
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            assert_ne!(c_id, a_id);
            assert_eq!(connects.load(Ordering::SeqCst), 3);
            for id in [&b_id, &c_id] {
                assert_eq!(
                    http.delete(&endpoint)
                        .header("mcp-session-id", id)
                        .send()
                        .await
                        .unwrap()
                        .status(),
                    reqwest::StatusCode::ACCEPTED
                );
            }
            assert_eq!(observed.disconnect_count.load(Ordering::SeqCst), 3);
            assert_eq!(
                observed
                    .native_messages
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, method)| method == "initialize")
                    .count(),
                3
            );
            Ok(())
        },
    )
    .await
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
