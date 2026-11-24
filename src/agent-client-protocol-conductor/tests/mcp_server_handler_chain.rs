//! Test that an MCP server doesn't break the handler chain for `NewSessionRequest`.
//!
//! This is a regression test for a bug where `McpServer::handle_dispatch` would
//! forward `NewSessionRequest` directly to the agent instead of returning
//! `Handled::No`, which prevented downstream `.on_receive_request_from()` handlers
//! from being invoked.

use agent_client_protocol::mcp_server::McpServer;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, InitializeRequest, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, McpServer as SchemaMcpServer, McpServerAcpId, MessageMcpRequest,
    MessageMcpResponse, NewSessionRequest, NewSessionResponse, ResumeSessionRequest,
    ResumeSessionResponse, SessionId,
};
use agent_client_protocol::{Agent, Client, Conductor, ConnectTo, DynConnectTo, Proxy};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use agent_client_protocol_rmcp::rmcp_3::McpServerExt as _;
use futures::{StreamExt as _, channel::mpsc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Simple echo tool parameters
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct EchoParams {
    message: String,
}

/// Simple echo tool output
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct EchoOutput {
    result: String,
}

/// Test helper to receive a JSON-RPC response
async fn recv<T: agent_client_protocol::JsonRpcResponse + Send>(
    response: agent_client_protocol::v1::SentRequest<T>,
) -> Result<T, agent_client_protocol::Error> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    response.on_receiving_result(async move |result| {
        tx.send(result)
            .map_err(|_| agent_client_protocol::Error::internal_error())
    })?;
    rx.await
        .map_err(|_| agent_client_protocol::Error::internal_error())?
}

/// Tracks whether the `NewSessionRequest` handler was invoked.
struct HandlerConfig {
    new_session_handler_called: AtomicBool,
}

#[derive(Default)]
struct SetupRequests {
    server_ids: Mutex<Vec<McpServerAcpId>>,
}

impl SetupRequests {
    fn record(&self, mcp_servers: &[SchemaMcpServer]) {
        let server_id = mcp_servers.iter().find_map(|server| match server {
            SchemaMcpServer::Acp(server) if server.name == "test-server" => {
                Some(server.server_id.clone())
            }
            _ => None,
        });

        if let Some(server_id) = server_id {
            self.server_ids.lock().unwrap().push(server_id);
        }
    }
}

impl HandlerConfig {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            new_session_handler_called: AtomicBool::new(false),
        })
    }

    fn was_handler_called(&self) -> bool {
        self.new_session_handler_called.load(Ordering::SeqCst)
    }
}

/// A proxy with both an MCP server and a `NewSessionRequest` handler.
/// When both were present, the request handler was never called.
struct ProxyWithMcpAndHandler {
    config: Arc<HandlerConfig>,
}

impl ConnectTo<Conductor> for ProxyWithMcpAndHandler {
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol::Error> {
        let config = Arc::clone(&self.config);

        // Create an MCP server with a simple tool
        let mcp_server = McpServer::builder("test-server".to_string())
            .instructions("A test MCP server")
            .tool_fn_mut(
                "echo",
                "Echoes back the input",
                async |params: EchoParams, _cx| {
                    Ok(EchoOutput {
                        result: format!("Echo: {}", params.message),
                    })
                },
                agent_client_protocol::tool_fn_mut!(),
            )
            .build();

        agent_client_protocol::Proxy
            .v1()
            .name("proxy-with-mcp-and-handler")
            // Add the MCP server
            .with_mcp_server(mcp_server)
            // Add a NewSessionRequest handler - this should be invoked!
            .on_receive_request_from(
                Client,
                async move |request: NewSessionRequest, responder, cx| {
                    // Mark that we were called
                    config
                        .new_session_handler_called
                        .store(true, Ordering::SeqCst);

                    // Forward to agent and relay response
                    cx.send_request_to(Agent, request)
                        .on_receiving_result(async move |result| {
                            let response: NewSessionResponse = result?;
                            responder.respond(response)
                        })
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}

/// A simple agent that responds to initialization and session requests
struct SimpleAgent {
    setup_requests: Arc<SetupRequests>,
}

impl ConnectTo<Client> for SimpleAgent {
    async fn connect_to(
        self,
        client: impl ConnectTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error> {
        let new_requests = self.setup_requests.clone();
        let load_requests = self.setup_requests.clone();
        let resume_requests = self.setup_requests;

        Agent
            .v1()
            .name("simple-agent")
            .on_receive_request(
                async |request: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(request.protocol_version)
                            .agent_capabilities(AgentCapabilities::new()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: NewSessionRequest, responder, _cx| {
                    new_requests.record(&request.mcp_servers);
                    responder.respond(NewSessionResponse::new(SessionId::new(
                        uuid::Uuid::new_v4().to_string(),
                    )))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: LoadSessionRequest, responder, _cx| {
                    load_requests.record(&request.mcp_servers);
                    responder.respond(LoadSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: ResumeSessionRequest, responder, _cx| {
                    resume_requests.record(&request.mcp_servers);
                    responder.respond(ResumeSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}

async fn run_test(
    proxies: Vec<DynConnectTo<Conductor>>,
    agent: DynConnectTo<Client>,
    editor_task: impl AsyncFnOnce(
        agent_client_protocol::v1::ConnectionTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error>,
) -> Result<(), agent_client_protocol::Error> {
    let (editor_out, conductor_in) = duplex(1024);
    let (conductor_out, editor_in) = duplex(1024);

    let transport =
        agent_client_protocol::ByteStreams::new(editor_out.compat_write(), editor_in.compat());

    agent_client_protocol::Client
        .v1()
        .name("editor-to-conductor")
        .with_spawned(|_cx| async move {
            ConductorImpl::new_agent(
                "conductor".to_string(),
                ProxiesAndAgent::new(agent).proxies(proxies),
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

/// The `NewSessionRequest` handler should be invoked even when an MCP server is present.
#[tokio::test]
async fn test_new_session_handler_invoked_with_mcp_server()
-> Result<(), agent_client_protocol::Error> {
    let handler_config = HandlerConfig::new();
    let handler_config_clone = Arc::clone(&handler_config);

    let proxy = DynConnectTo::<Conductor>::new(ProxyWithMcpAndHandler {
        config: handler_config,
    });
    let agent = DynConnectTo::<Client>::new(SimpleAgent {
        setup_requests: Arc::default(),
    });

    run_test(vec![proxy], agent, async |connection_to_editor| {
        // Initialize first
        let _init_response =
            recv(connection_to_editor.send_request(InitializeRequest::new(ProtocolVersion::V1)))
                .await?;

        // Create a new session - this should trigger the handler in the proxy
        let session_response =
            recv(connection_to_editor.send_request(NewSessionRequest::new(PathBuf::from("/tmp"))))
                .await?;

        // Verify we got a valid session ID
        assert!(
            !session_response.session_id.0.is_empty(),
            "Should receive a valid session ID"
        );

        Ok::<(), agent_client_protocol::Error>(())
    })
    .await?;

    // THE KEY ASSERTION: verify the handler was actually called
    assert!(
        handler_config_clone.was_handler_called(),
        "NewSessionRequest handler should be invoked even when MCP server is in the chain. \
         This is a regression - the MCP server was incorrectly forwarding the request directly \
         to the agent instead of letting it flow through the handler chain."
    );

    Ok(())
}

/// Global MCP servers are advertised consistently on every stable session setup request.
#[tokio::test]
async fn test_mcp_server_injected_into_all_session_setup_requests()
-> Result<(), agent_client_protocol::Error> {
    let handler_config = HandlerConfig::new();
    let setup_requests = Arc::new(SetupRequests::default());

    let proxy = DynConnectTo::<Conductor>::new(ProxyWithMcpAndHandler {
        config: handler_config,
    });
    let agent = DynConnectTo::<Client>::new(SimpleAgent {
        setup_requests: setup_requests.clone(),
    });

    run_test(vec![proxy], agent, async |connection_to_editor| {
        recv(connection_to_editor.send_request(InitializeRequest::new(ProtocolVersion::V1)))
            .await?;

        let cwd = PathBuf::from("/tmp");
        let new_session =
            recv(connection_to_editor.send_request(NewSessionRequest::new(cwd.clone()))).await?;

        recv(connection_to_editor.send_request(LoadSessionRequest::new(
            new_session.session_id.clone(),
            cwd.clone(),
        )))
        .await?;

        recv(
            connection_to_editor
                .send_request(ResumeSessionRequest::new(new_session.session_id, cwd)),
        )
        .await?;

        Ok::<(), agent_client_protocol::Error>(())
    })
    .await?;

    let server_ids = setup_requests.server_ids.lock().unwrap();
    assert_eq!(
        server_ids.len(),
        3,
        "each setup request should advertise the server"
    );
    assert!(
        server_ids.windows(2).all(|ids| ids[0] == ids[1]),
        "a global MCP server should keep one advertised server ID"
    );

    Ok(())
}

/// A proxy whose globally attached MCP server needs the session of every
/// request.
struct ProxyWithGatedMcp {
    tool_runs: Arc<AtomicUsize>,
}

impl ConnectTo<Conductor> for ProxyWithGatedMcp {
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol::Error> {
        let tool_runs = self.tool_runs;
        let mcp_server = McpServer::builder("test-server".to_string())
            .tool_fn(
                "echo",
                "Echoes back the input",
                async move |params: EchoParams, _cx| {
                    tool_runs.fetch_add(1, Ordering::SeqCst);
                    Ok(EchoOutput {
                        result: params.message,
                    })
                },
                agent_client_protocol::tool_fn!(),
            )
            .build()
            .with_admission(
                async |request| {
                    request.require_session_id()?;
                    Ok(())
                },
                agent_client_protocol::mcp_admission!(),
            );

        agent_client_protocol::Proxy
            .v1()
            .name("proxy-with-gated-mcp")
            .with_mcp_server(mcp_server)
            .connect_to(client)
            .await
    }
}

type McpCallOutcome = Result<MessageMcpResponse, agent_client_protocol::Error>;

/// An agent that calls the `echo` tool of the injected server twice for each
/// new session: without a session, then for the session `child`.
struct CallingAgent {
    outcomes: mpsc::UnboundedSender<McpCallOutcome>,
}

impl ConnectTo<Client> for CallingAgent {
    async fn connect_to(
        self,
        client: impl ConnectTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error> {
        let outcomes = self.outcomes;
        Agent
            .v1()
            .name("calling-agent")
            .on_receive_request(
                async |request: InitializeRequest, responder, _cx| {
                    responder.respond(InitializeResponse::new(request.protocol_version))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: NewSessionRequest, responder, cx| {
                    let server_id = request
                        .mcp_servers
                        .iter()
                        .find_map(|server| match server {
                            SchemaMcpServer::Acp(server) => Some(server.server_id.clone()),
                            _ => None,
                        })
                        .expect("the proxy injects its MCP server");
                    let outcomes = outcomes.clone();
                    let mcp_cx = cx.clone();
                    cx.spawn(async move {
                        let params = serde_json::json!({
                            "name": "echo",
                            "arguments": {"message": "hello"},
                            "_meta": {
                                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                                "io.modelcontextprotocol/clientCapabilities": {}
                            }
                        });
                        for session_id in [None, Some(SessionId::new("child"))] {
                            let request = MessageMcpRequest::new(
                                server_id.clone(),
                                "echo-call",
                                "tools/call",
                            )
                            .params(params.as_object().unwrap().clone())
                            .session_id(session_id);
                            let outcome = mcp_cx.send_request(request).block_task().await;
                            outcomes
                                .unbounded_send(outcome)
                                .map_err(agent_client_protocol::Error::into_internal_error)?;
                        }
                        Ok(())
                    })?;
                    responder.respond(NewSessionResponse::new(SessionId::new("root")))
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
async fn test_global_mcp_server_admission_check_rejects_before_the_tool_runs()
-> Result<(), agent_client_protocol::Error> {
    let tool_runs = Arc::new(AtomicUsize::new(0));
    let (outcomes_tx, mut outcomes) = mpsc::unbounded();
    let proxy = DynConnectTo::<Conductor>::new(ProxyWithGatedMcp {
        tool_runs: tool_runs.clone(),
    });
    let agent = DynConnectTo::<Client>::new(CallingAgent {
        outcomes: outcomes_tx,
    });

    run_test(vec![proxy], agent, async |connection_to_editor| {
        recv(connection_to_editor.send_request(InitializeRequest::new(ProtocolVersion::V1)))
            .await?;
        recv(connection_to_editor.send_request(NewSessionRequest::new(PathBuf::from("/tmp"))))
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
            serde_json::json!({
                "code": -32602,
                "message": "Invalid params",
                "data": "this MCP server requires sessionId on mcp/message",
            })
        );

        let MessageMcpResponse::Result { result, .. } = next().await? else {
            panic!("the attributed call should return a tool result")
        };
        assert_eq!(result["structuredContent"]["result"], "hello");
        // Two calls, and the tool ran for the admitted one only.
        assert_eq!(tool_runs.load(Ordering::SeqCst), 1);
        Ok::<(), agent_client_protocol::Error>(())
    })
    .await
}
