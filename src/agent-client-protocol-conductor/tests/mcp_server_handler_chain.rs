//! Test that MCP server doesn't break the handler chain for NewSessionRequest.
//!
//! This is a regression test for a bug where `McpServer::handle_dispatch` would
//! forward `NewSessionRequest` directly to the agent instead of returning
//! `Handled::No`, which prevented downstream `.on_receive_request_from()` handlers
//! from being invoked.

use agent_client_protocol::mcp_server::McpServer;
use agent_client_protocol::schema::{
    AgentCapabilities, InitializeRequest, InitializeResponse, NewSessionRequest,
    NewSessionResponse, ProtocolVersion, SessionId,
};
use agent_client_protocol::{Agent, Client, Conductor, ConnectTo, DynConnectTo, Proxy};
use agent_client_protocol_conductor::{ConductorImpl, McpBridgeMode, ProxiesAndAgent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Tracks whether the NewSessionRequest handler was invoked
struct HandlerConfig {
    new_session_handler_called: AtomicBool,
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

/// A proxy component that has BOTH an MCP server AND a NewSessionRequest handler.
/// The bug was that when both were present, the NewSessionRequest handler was never called.
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
            .builder()
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
struct SimpleAgent;

impl ConnectTo<Client> for SimpleAgent {
    async fn connect_to(
        self,
        client: impl ConnectTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error> {
        Agent
            .builder()
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
                async |_request: NewSessionRequest, responder, _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new(
                        uuid::Uuid::new_v4().to_string(),
                    )))
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
        agent_client_protocol::ConnectionTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error>,
) -> Result<(), agent_client_protocol::Error> {
    let (editor_out, conductor_in) = duplex(1024);
    let (conductor_out, editor_in) = duplex(1024);

    let transport =
        agent_client_protocol::ByteStreams::new(editor_out.compat_write(), editor_in.compat());

    agent_client_protocol::Client
        .builder()
        .name("editor-to-conductor")
        .with_spawned(|_cx| async move {
            ConductorImpl::new_agent(
                "conductor".to_string(),
                ProxiesAndAgent::new(agent).proxies(proxies),
                McpBridgeMode::default(),
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

/// Regression test: NewSessionRequest handler should be invoked even when MCP server is present
#[tokio::test]
async fn test_new_session_handler_invoked_with_mcp_server()
-> Result<(), agent_client_protocol::Error> {
    let handler_config = HandlerConfig::new();
    let handler_config_clone = Arc::clone(&handler_config);

    let proxy = DynConnectTo::<Conductor>::new(ProxyWithMcpAndHandler {
        config: handler_config,
    });
    let agent = DynConnectTo::<Client>::new(SimpleAgent);

    run_test(vec![proxy], agent, async |connection_to_editor| {
        // Initialize first
        let _init_response = recv(
            connection_to_editor.send_request(InitializeRequest::new(ProtocolVersion::LATEST)),
        )
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
