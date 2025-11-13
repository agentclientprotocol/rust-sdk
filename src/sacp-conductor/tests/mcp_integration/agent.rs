//! Agent component that verifies MCP server configuration and handles prompts

use rmcp::ServiceExt;
use sacp::schema::{
    AgentCapabilities, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    McpServer, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    SessionNotification, SessionUpdate, StopReason, TextContent,
};
use sacp::{Component, JrHandlerChain, JrRequestCx};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::conductor_command;

pub struct AgentComponent;

/// Shared state for the agent component
#[derive(Clone)]
struct AgentState {
    /// MCP servers available in the session
    mcp_servers: Arc<Mutex<Vec<McpServer>>>,
}

impl Component for AgentComponent {
    async fn serve(self, client: impl Component) -> Result<(), sacp::Error> {
        let state = AgentState {
            mcp_servers: Arc::new(Mutex::new(Vec::new())),
        };

        JrHandlerChain::new()
            .name("agent-component")
            .on_receive_request(async move |request: InitializeRequest, request_cx| {
                // Simple initialization response
                let response = InitializeResponse {
                    protocol_version: request.protocol_version,
                    agent_capabilities: AgentCapabilities::default(),
                    auth_methods: vec![],
                    meta: None,
                    agent_info: None,
                };
                request_cx.respond(response)
            })
            .on_receive_request({
                let state = state.clone();
                async move |request: NewSessionRequest, request_cx| {
                    assert_eq!(request.mcp_servers.len(), 1);

                    // Although the proxy injects an HTTP server, it will be rewritten to stdio by the conductor.
                    let mcp_server = &request.mcp_servers[0];
                    assert!(
                        matches!(mcp_server, McpServer::Stdio { .. }),
                        "expected a stdio MCP server: {:?}",
                        request.mcp_servers
                    );

                    // Verify the stdio configuration is correct
                    if let McpServer::Stdio {
                        name,
                        command,
                        args,
                        ..
                    } = mcp_server
                    {
                        assert_eq!(name, "test");
                        let conductor_command = conductor_command();
                        assert_eq!(command.to_str().unwrap(), &conductor_command[0]);
                        for (arg, expected) in args.iter().zip(&conductor_command[1..]) {
                            assert_eq!(arg, expected);
                        }
                    }

                    // Store MCP servers for later use
                    *state.mcp_servers.lock().await = request.mcp_servers;

                    // Simple session response
                    let response = NewSessionResponse {
                        session_id: "test-session-123".into(),
                        modes: None,
                        meta: None,
                    };
                    request_cx.respond(response)
                }
            })
            .on_receive_request({
                let state = state.clone();
                async move |request: PromptRequest, request_cx| {
                    tracing::debug!(
                        session_id = %request.session_id.0,
                        "Received prompt request"
                    );

                    // Run the rest out of turn so the loop stays responsive
                    let connection_cx = request_cx.connection_cx();
                    let state = state.clone();
                    connection_cx.spawn(Self::respond_to_prompt(state, request, request_cx))
                }
            })
            .serve(client)
            .await
    }
}

impl AgentComponent {
    async fn respond_to_prompt(
        state: AgentState,
        request: PromptRequest,
        request_cx: JrRequestCx<PromptResponse>,
    ) -> Result<(), sacp::Error> {
        use rmcp::{
            model::CallToolRequestParam,
            transport::{ConfigureCommandExt, TokioChildProcess},
        };
        use tokio::process::Command;

        let connection_cx = request_cx.connection_cx();

        // Send initial message
        connection_cx.send_notification(SessionNotification {
            session_id: request.session_id.clone(),
            update: SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent {
                    annotations: None,
                    text: "Hello. I will now use the MCP tool".to_string(),
                    meta: None,
                }),
                meta: None,
            }),
            meta: None,
        })?;

        // Get MCP servers
        let mcp_servers = state.mcp_servers.lock().await;
        if let Some(mcp_server) = mcp_servers.first() {
            if let McpServer::Stdio {
                command, args, env, ..
            } = mcp_server
            {
                tracing::debug!(
                    command = ?command,
                    args = ?args,
                    "Starting MCP client"
                );

                // Create MCP client by spawning the process
                let mcp_client = ()
                    .serve(
                        TokioChildProcess::new(Command::new(command).configure(|cmd| {
                            cmd.args(args);
                            for env_var in env {
                                cmd.env(&env_var.name, &env_var.value);
                            }
                        }))
                        .map_err(sacp::Error::into_internal_error)?,
                    )
                    .await
                    .map_err(sacp::Error::into_internal_error)?;

                tracing::debug!("MCP client connected");

                // Call the echo tool
                let tool_result = mcp_client
                    .call_tool(CallToolRequestParam {
                        name: "echo".into(),
                        arguments: serde_json::json!({
                            "message": "Hello from the agent!"
                        })
                        .as_object()
                        .cloned(),
                    })
                    .await
                    .map_err(sacp::Error::into_internal_error)?;

                tracing::debug!("Tool call result: {:?}", tool_result);

                // Send the tool result as a message
                connection_cx.send_notification(SessionNotification {
                    session_id: request.session_id.clone(),
                    update: SessionUpdate::AgentMessageChunk(ContentChunk {
                        content: ContentBlock::Text(TextContent {
                            annotations: None,
                            text: format!("MCP tool result: {:?}", tool_result),
                            meta: None,
                        }),
                        meta: None,
                    }),
                    meta: None,
                })?;

                // Clean up the client
                mcp_client
                    .cancel()
                    .await
                    .map_err(sacp::Error::into_internal_error)?;
            }
        }

        let response = PromptResponse {
            stop_reason: StopReason::EndTurn,
            meta: None,
        };

        request_cx.respond(response)
    }
}

pub fn create() -> sacp::DynComponent {
    sacp::DynComponent::new(AgentComponent)
}
