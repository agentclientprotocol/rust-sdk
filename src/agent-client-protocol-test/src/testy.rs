//! testy: you friendly neighborhood ACP test agent with typed JSON commands.
//!
//! The agent accepts JSON-serialized [`TestyCommand`] values as prompt text.

use agent_client_protocol::schema::{
    AgentCapabilities, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    McpServer, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionId,
    SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Responder};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Commands that can be sent as prompt text (serialized as JSON) to the [`Testy`].
///
/// Tests construct these as typed values and serialize to JSON via [`TestyCommand::to_prompt`].
/// The agent deserializes the prompt text and dispatches accordingly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum TestyCommand {
    /// Responds with `"Hello, world!"`.
    Greet,

    /// Echoes the given message back as the response.
    Echo { message: String },

    /// Invokes an MCP tool and returns the result.
    /// The agent must have been given MCP servers in the `NewSessionRequest`.
    CallTool {
        server: String,
        tool: String,
        #[serde(default)]
        params: serde_json::Value,
    },

    /// Lists tools from the named MCP server.
    ListTools { server: String },
}

impl TestyCommand {
    /// Serialize this command to a JSON string suitable for use as prompt text.
    #[must_use]
    pub fn to_prompt(&self) -> String {
        serde_json::to_string(self).expect("TestyCommand serialization should not fail")
    }
}

/// Session data for each active session.
#[derive(Clone, Debug)]
struct SessionData {
    mcp_servers: Vec<McpServer>,
}

/// A minimal ACP test agent.
///
/// Implements `ConnectTo<Client>` and handles `InitializeRequest`, `NewSessionRequest`,
/// and `PromptRequest`. Prompt text is parsed as a JSON [`TestyCommand`]; if parsing fails,
/// the agent responds with `"Hello, world!"` (equivalent to [`TestyCommand::Greet`]).
#[derive(Clone, Debug)]
pub struct Testy {
    sessions: Arc<Mutex<HashMap<SessionId, SessionData>>>,
}

impl Testy {
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn create_session(&self, session_id: &SessionId, mcp_servers: Vec<McpServer>) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.insert(session_id.clone(), SessionData { mcp_servers });
    }

    fn get_mcp_servers(&self, session_id: &SessionId) -> Option<Vec<McpServer>> {
        let sessions = self.sessions.lock().unwrap();
        sessions
            .get(session_id)
            .map(|session| session.mcp_servers.clone())
    }

    async fn process_prompt(
        &self,
        request: PromptRequest,
        responder: Responder<PromptResponse>,
        connection: ConnectionTo<Client>,
    ) -> Result<(), agent_client_protocol::Error> {
        let session_id = request.session_id.clone();
        let input_text = extract_text_from_prompt(&request.prompt);

        let command: TestyCommand =
            serde_json::from_str(&input_text).unwrap_or(TestyCommand::Greet);

        let response_text = match command {
            TestyCommand::Greet => "Hello, world!".to_string(),

            TestyCommand::Echo { message } => message,

            TestyCommand::CallTool {
                server,
                tool,
                params,
            } => match self
                .execute_tool_call(&session_id, &server, &tool, params)
                .await
            {
                Ok(result) => format!("OK: {result}"),
                Err(e) => format!("ERROR: {e}"),
            },

            TestyCommand::ListTools { server } => {
                match self.list_tools(&session_id, &server).await {
                    Ok(tools) => format!("Available tools:\n{tools}"),
                    Err(e) => format!("ERROR: {e}"),
                }
            }
        };

        connection.send_notification(SessionNotification::new(
            session_id,
            SessionUpdate::AgentMessageChunk(ContentChunk::new(response_text.into())),
        ))?;

        responder.respond(PromptResponse::new(StopReason::EndTurn))
    }

    /// Helper to execute an operation with a spawned MCP client.
    async fn with_mcp_client<F, Fut, T>(
        &self,
        session_id: &SessionId,
        server_name: &str,
        operation: F,
    ) -> Result<T>
    where
        F: FnOnce(rmcp::service::RunningService<rmcp::RoleClient, ()>) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        use rmcp::{
            ServiceExt,
            transport::{ConfigureCommandExt, TokioChildProcess},
        };
        use tokio::process::Command;

        let mcp_servers = self
            .get_mcp_servers(session_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found"))?;

        let mcp_server = mcp_servers
            .iter()
            .find(|server| match server {
                McpServer::Stdio(stdio) => stdio.name == server_name,
                McpServer::Http(http) => http.name == server_name,
                McpServer::Sse(sse) => sse.name == server_name,
                _ => false,
            })
            .ok_or_else(|| anyhow::anyhow!("MCP server '{server_name}' not found"))?;

        match mcp_server {
            McpServer::Stdio(stdio) => {
                let mcp_client = ()
                    .serve(TokioChildProcess::new(
                        Command::new(&stdio.command).configure(|cmd| {
                            cmd.args(&stdio.args);
                            for env_var in &stdio.env {
                                cmd.env(&env_var.name, &env_var.value);
                            }
                        }),
                    )?)
                    .await?;

                operation(mcp_client).await
            }
            McpServer::Http(http) => {
                use rmcp::transport::StreamableHttpClientTransport;

                let mcp_client =
                    ().serve(StreamableHttpClientTransport::from_uri(http.url.as_str()))
                        .await?;

                operation(mcp_client).await
            }
            McpServer::Sse(_) => Err(anyhow::anyhow!("SSE MCP servers not yet supported")),
            _ => Err(anyhow::anyhow!("Unknown MCP server type")),
        }
    }

    async fn list_tools(&self, session_id: &SessionId, server_name: &str) -> Result<String> {
        self.with_mcp_client(session_id, server_name, async move |mcp_client| {
            let tools_result = mcp_client.list_tools(None).await?;
            mcp_client.cancel().await?;

            let tools_list = tools_result
                .tools
                .iter()
                .map(|tool| {
                    format!(
                        "  - {}: {}",
                        tool.name,
                        tool.description.as_deref().unwrap_or("No description")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");

            Ok(tools_list)
        })
        .await
    }

    async fn execute_tool_call(
        &self,
        session_id: &SessionId,
        server_name: &str,
        tool_name: &str,
        params: serde_json::Value,
    ) -> Result<String> {
        use rmcp::model::CallToolRequestParams;

        let params_obj = params.as_object().cloned().unwrap_or_default();
        let tool_name = tool_name.to_string();

        self.with_mcp_client(session_id, server_name, async move |mcp_client| {
            let tool_result = mcp_client
                .call_tool(CallToolRequestParams::new(tool_name).with_arguments(params_obj))
                .await?;

            mcp_client.cancel().await?;

            Ok(format!("{tool_result:?}"))
        })
        .await
    }
}

impl Default for Testy {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract text content from prompt blocks.
fn extract_text_from_prompt(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(TextContent { text, .. }) => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl ConnectTo<Client> for Testy {
    async fn connect_to(
        self,
        client: impl ConnectTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error> {
        Agent
            .builder()
            .name("test-agent")
            .on_receive_request(
                async |initialize: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(initialize.protocol_version)
                            .agent_capabilities(AgentCapabilities::new()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: NewSessionRequest, responder, _cx| {
                        let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
                        agent.create_session(&session_id, request.mcp_servers);
                        responder.respond(NewSessionResponse::new(session_id))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: PromptRequest, responder, cx| {
                        let cx_clone = cx.clone();
                        cx.spawn({
                            let agent = agent.clone();
                            async move { agent.process_prompt(request, responder, cx_clone).await }
                        })
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}
