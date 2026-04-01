//! Cookbook of common patterns for building ACP components.
//!
//! This crate contains guides and examples for the three main things you can build with ACP:
//!
//! - **Clients** - Connect to an existing agent and send prompts
//! - **Proxies** - Sit between client and agent to add capabilities (like MCP tools)
//! - **Agents** - Respond to prompts with AI-powered responses
//!
//! See the [`agent_client_protocol_core::concepts`] module for detailed explanations of
//! the concepts behind the API.
//!
//! # Building Clients
//!
//! A client connects to an agent, sends requests, and handles responses. Use
//! [`Client.builder()`](agent_client_protocol_core::Client) to build connections.
//!
//! - [`one_shot_prompt`] - Send a single prompt and get a response (simplest pattern)
//! - [`connecting_as_client`] - More details on connection setup and permission handling
//!
//! # Building Proxies
//!
//! A proxy sits between client and agent, intercepting and optionally modifying
//! messages. The most common use case is adding MCP tools. Use [`Proxy.builder()`](agent_client_protocol_core::Proxy)
//! to build proxy connections.
//!
//! **Important:** Proxies don't run standalone—they need the [`agent-client-protocol-conductor`] to
//! orchestrate the connection between client, proxies, and agent. See
//! [`running_proxies_with_conductor`] for how to put the pieces together.
//!
//! - [`global_mcp_server`] - Add tools that work across all sessions
//! - [`per_session_mcp_server`] - Add tools with session-specific state
//! - [`filtering_tools`] - Enable or disable tools dynamically
//! - [`reusable_components`] - Package your proxy as a [`ConnectTo`] for composition
//! - [`running_proxies_with_conductor`] - Run your proxy with an agent
//!
//! [`agent-client-protocol-conductor`]: https://crates.io/crates/agent-client-protocol-conductor
//!
//! # Building Agents
//!
//! An agent receives prompts and generates responses. Use [`Agent.builder()`](agent_client_protocol_core::Agent)
//! to build agent connections.
//!
//! - [`building_an_agent`] - Handle initialization, sessions, and prompts
//! - [`reusable_components`] - Package your agent as a [`ConnectTo`]
//! - [`custom_message_handlers`] - Fine-grained control over message routing
//!
//! [`agent_client_protocol_core::concepts`]: agent_client_protocol_core::concepts
//! [`Client`]: agent_client_protocol_core::Client
//! [`Agent`]: agent_client_protocol_core::Agent
//! [`Proxy`]: agent_client_protocol_core::Proxy
//! [`ConnectTo`]: agent_client_protocol_core::ConnectTo

pub mod one_shot_prompt {
    //! Pattern: You Only Prompt Once.
    //!
    //! The simplest client pattern: connect to an agent, send one prompt, get the
    //! response. This is useful for CLI tools, scripts, or any case where you just
    //! need a single interaction with an agent.
    //!
    //! # Example
    //!
    //! ```
    //! use agent_client_protocol_core::{Client, Agent, ConnectTo};
    //! use agent_client_protocol_core::schema::{InitializeRequest, ProtocolVersion};
    //!
    //! async fn ask_agent(
    //!     transport: impl ConnectTo<Client> + 'static,
    //!     prompt: &str,
    //! ) -> Result<String, agent_client_protocol_core::Error> {
    //!     Client.builder()
    //!         .name("my-client")
    //!         .connect_with(transport, async |connection| {
    //!             // Initialize the connection
    //!             connection.send_request(InitializeRequest::new(ProtocolVersion::LATEST))
    //!                 .block_task().await?;
    //!
    //!             // Create a session, send prompt, read response
    //!             let mut session = connection.build_session_cwd()?
    //!                 .block_task()
    //!                 .start_session()
    //!                 .await?;
    //!
    //!             session.send_prompt(prompt)?;
    //!             session.read_to_string().await
    //!         })
    //!         .await
    //! }
    //! ```
    //!
    //! # How it works
    //!
    //! 1. **[`connect_with`]** establishes the transport connection and runs your
    //!    code while handling messages in the background
    //! 2. **[`send_request`]** + **[`block_task`]** sends the initialize request
    //!    and waits for the response
    //! 3. **[`build_session_cwd`]** creates a session builder using the current working directory
    //! 4. **[`start_session`]** sends the `NewSessionRequest` and returns an
    //!    [`ActiveSession`] handle
    //! 5. **[`send_prompt`]** queues the prompt to send to the agent
    //! 6. **[`read_to_string`]** reads all text chunks until the agent finishes
    //!
    //! # Handling permission requests
    //!
    //! Most agents will ask for permission before taking actions like running
    //! commands or writing files. See [`connecting_as_client`] for how to handle
    //! [`RequestPermissionRequest`] messages.
    //!
    //! [`connect_with`]: agent_client_protocol_core::Builder::connect_with
    //! [`send_request`]: agent_client_protocol_core::ConnectionTo::send_request
    //! [`block_task`]: agent_client_protocol_core::SentRequest::block_task
    //! [`build_session_cwd`]: agent_client_protocol_core::ConnectionTo::build_session_cwd
    //! [`start_session`]: agent_client_protocol_core::SessionBuilder::start_session
    //! [`ActiveSession`]: agent_client_protocol_core::ActiveSession
    //! [`send_prompt`]: agent_client_protocol_core::ActiveSession::send_prompt
    //! [`read_to_string`]: agent_client_protocol_core::ActiveSession::read_to_string
    //! [`connecting_as_client`]: super::connecting_as_client
    //! [`RequestPermissionRequest`]: agent_client_protocol_core::schema::RequestPermissionRequest
}

pub mod connecting_as_client {
    //! Pattern: Connecting as a client.
    //!
    //! To connect to an ACP agent and send requests, use [`connect_with`].
    //! This runs your code while the connection handles incoming messages
    //! in the background.
    //!
    //! # Basic Example
    //!
    //! ```
    //! use agent_client_protocol_core::{Client, Agent, ConnectTo};
    //! use agent_client_protocol_core::schema::{InitializeRequest, ProtocolVersion};
    //!
    //! async fn connect_to_agent(transport: impl ConnectTo<Client>) -> Result<(), agent_client_protocol_core::Error> {
    //!     Client.builder()
    //!         .name("my-client")
    //!         .connect_with(transport, async |connection| {
    //!             // Initialize the connection
    //!             connection.send_request(InitializeRequest::new(ProtocolVersion::LATEST))
    //!                 .block_task().await?;
    //!
    //!             // Create a session and send a prompt
    //!             connection.build_session_cwd()?
    //!                 .block_task()
    //!                 .run_until(async |mut session| {
    //!                     session.send_prompt("Hello, agent!")?;
    //!                     let response = session.read_to_string().await?;
    //!                     println!("Agent said: {}", response);
    //!                     Ok(())
    //!                 })
    //!                 .await
    //!         })
    //!         .await
    //! }
    //! ```
    //!
    //! # Using the Session Builder
    //!
    //! The [`build_session`] method creates a [`SessionBuilder`] that handles
    //! session creation and provides convenient methods for interacting with
    //! the session:
    //!
    //! - [`send_prompt`] - Send a text prompt to the agent
    //! - [`read_update`] - Read the next update (text chunk, tool call, etc.)
    //! - [`read_to_string`] - Read all text until the turn ends
    //!
    //! The session builder also supports adding MCP servers with [`with_mcp_server`].
    //!
    //! # Handling Permission Requests
    //!
    //! Agents may send [`RequestPermissionRequest`] to ask for user approval
    //! before taking actions. Handle these with [`on_receive_request`]:
    //!
    //! ```ignore
    //! Client.builder()
    //!     .on_receive_request(async |req: RequestPermissionRequest, responder, _connection| {
    //!         // Auto-approve by selecting the first option (YOLO mode)
    //!         let option_id = req.options.first().map(|opt| opt.id.clone());
    //!         responder.respond(RequestPermissionResponse {
    //!             outcome: match option_id {
    //!                 Some(id) => RequestPermissionOutcome::Selected { option_id: id },
    //!                 None => RequestPermissionOutcome::Cancelled,
    //!             },
    //!             meta: None,
    //!         })
    //!     }, agent_client_protocol_core::on_receive_request!())
    //!     .connect_with(transport, async |connection| { /* ... */ })
    //!     .await
    //! ```
    //!
    //! # Note on `block_task`
    //!
    //! Using [`block_task`] is safe inside `connect_with` because the closure runs
    //! as a spawned task, not on the event loop. The event loop continues processing
    //! messages (including the response you're waiting for) while your task blocks.
    //!
    //! [`connect_with`]: agent_client_protocol_core::Builder::connect_with
    //! [`block_task`]: agent_client_protocol_core::SentRequest::block_task
    //! [`build_session`]: agent_client_protocol_core::ConnectionTo::build_session
    //! [`SessionBuilder`]: agent_client_protocol_core::SessionBuilder
    //! [`send_prompt`]: agent_client_protocol_core::ActiveSession::send_prompt
    //! [`read_update`]: agent_client_protocol_core::ActiveSession::read_update
    //! [`read_to_string`]: agent_client_protocol_core::ActiveSession::read_to_string
    //! [`with_mcp_server`]: agent_client_protocol_core::SessionBuilder::with_mcp_server
    //! [`RequestPermissionRequest`]: agent_client_protocol_core::schema::RequestPermissionRequest
    //! [`on_receive_request`]: agent_client_protocol_core::Builder::on_receive_request
}

pub mod building_an_agent {
    //! Pattern: Building an agent.
    //!
    //! An agent handles prompts and generates responses. At minimum, an agent must:
    //!
    //! 1. Handle [`InitializeRequest`] to establish the connection
    //! 2. Handle [`NewSessionRequest`] to create sessions
    //! 3. Handle [`PromptRequest`] to process prompts
    //!
    //! Use [`Agent.builder()`](agent_client_protocol_core::Agent) to build agent connections.
    //!
    //! # Minimal Example
    //!
    //! ```
    //! use agent_client_protocol_core::{Agent, Client, ConnectTo, Dispatch, ConnectionTo};
    //! use agent_client_protocol_core::schema::{
    //!     InitializeRequest, InitializeResponse, AgentCapabilities,
    //!     NewSessionRequest, NewSessionResponse, SessionId,
    //!     PromptRequest, PromptResponse, StopReason,
    //! };
    //!
    //! async fn run_agent(transport: impl ConnectTo<Agent>) -> Result<(), agent_client_protocol_core::Error> {
    //!     Agent.builder()
    //!         .name("my-agent")
    //!         // Handle initialization
    //!         .on_receive_request(async |req: InitializeRequest, responder, _connection| {
    //!             responder.respond(
    //!                 InitializeResponse::new(req.protocol_version)
    //!                     .agent_capabilities(AgentCapabilities::new())
    //!             )
    //!         }, agent_client_protocol_core::on_receive_request!())
    //!         // Handle session creation
    //!         .on_receive_request(async |req: NewSessionRequest, responder, _connection| {
    //!             responder.respond(NewSessionResponse::new(SessionId::new("session-1")))
    //!         }, agent_client_protocol_core::on_receive_request!())
    //!         // Handle prompts
    //!         .on_receive_request(async |req: PromptRequest, responder, connection| {
    //!             // Send streaming updates via notifications
    //!             // connection.send_notification(SessionNotification { ... })?;
    //!
    //!             // Return final response
    //!             responder.respond(PromptResponse::new(StopReason::EndTurn))
    //!         }, agent_client_protocol_core::on_receive_request!())
    //!         // Reject unknown messages
    //!         .on_receive_dispatch(async |message: Dispatch, connection: ConnectionTo<Client>| {
    //!             message.respond_with_error(agent_client_protocol_core::Error::method_not_found(), connection)
    //!         }, agent_client_protocol_core::on_receive_dispatch!())
    //!         .connect_to(transport)
    //!         .await
    //! }
    //! ```
    //!
    //! # Streaming Responses
    //!
    //! To stream text or other updates to the client, send [`SessionNotification`]s
    //! while processing a prompt:
    //!
    //! ```ignore
    //! .on_receive_request(async |req: PromptRequest, responder, connection| {
    //!     // Stream some text
    //!     connection.send_notification(SessionNotification {
    //!         session_id: req.session_id.clone(),
    //!         update: SessionUpdate::Text(TextUpdate {
    //!             text: "Hello, ".into(),
    //!             // ...
    //!         }),
    //!         meta: None,
    //!     })?;
    //!
    //!     connection.send_notification(SessionNotification {
    //!         session_id: req.session_id.clone(),
    //!         update: SessionUpdate::Text(TextUpdate {
    //!             text: "world!".into(),
    //!             // ...
    //!         }),
    //!         meta: None,
    //!     })?;
    //!
    //!     responder.respond(PromptResponse {
    //!         stop_reason: StopReason::EndTurn,
    //!         meta: None,
    //!     })
    //! }, agent_client_protocol_core::on_receive_request!())
    //! ```
    //!
    //! # Requesting Permissions
    //!
    //! Before taking actions that require user approval (like running commands
    //! or writing files), send a [`RequestPermissionRequest`]:
    //!
    //! ```ignore
    //! let response = connection.send_request(RequestPermissionRequest {
    //!     session_id: session_id.clone(),
    //!     action: PermissionAction::Bash { command: "rm -rf /".into() },
    //!     options: vec![
    //!         PermissionOption { id: "allow".into(), label: "Allow".into() },
    //!         PermissionOption { id: "deny".into(), label: "Deny".into() },
    //!     ],
    //!     meta: None,
    //! }).block_task().await?;
    //!
    //! match response.outcome {
    //!     RequestPermissionOutcome::Selected { option_id } if option_id == "allow" => {
    //!         // User approved, proceed with action
    //!     }
    //!     _ => {
    //!         // User denied or cancelled
    //!     }
    //! }
    //! ```
    //!
    //! # As a Reusable Component
    //!
    //! For agents that will be composed with proxies, implement [`ConnectTo`].
    //! See [`reusable_components`] for the pattern.
    //!
    //! [`InitializeRequest`]: agent_client_protocol_core::schema::InitializeRequest
    //! [`NewSessionRequest`]: agent_client_protocol_core::schema::NewSessionRequest
    //! [`PromptRequest`]: agent_client_protocol_core::schema::PromptRequest
    //! [`SessionNotification`]: agent_client_protocol_core::schema::SessionNotification
    //! [`RequestPermissionRequest`]: agent_client_protocol_core::schema::RequestPermissionRequest
    //! [`Agent`]: agent_client_protocol_core::Agent
    //! [`ConnectTo`]: agent_client_protocol_core::ConnectTo
    //! [`reusable_components`]: super::reusable_components
}

pub mod reusable_components {
    //! Pattern: Defining reusable components.
    //!
    //! When building agents or proxies that will be composed together (for example,
    //! with [`agent-client-protocol-conductor`]), define a struct that implements [`ConnectTo`].
    //! This allows your component to be connected to other components in a type-safe way.
    //!
    //! # Example
    //!
    //! ```
    //! use agent_client_protocol_core::{ConnectTo, Agent, Client};
    //! use agent_client_protocol_core::schema::{
    //!     InitializeRequest, InitializeResponse, AgentCapabilities,
    //! };
    //!
    //! struct MyAgent {
    //!     name: String,
    //! }
    //!
    //! impl ConnectTo<Client> for MyAgent {
    //!     async fn connect_to(self, client: impl ConnectTo<Agent>) -> Result<(), agent_client_protocol_core::Error> {
    //!         Agent.builder()
    //!             .name(&self.name)
    //!             .on_receive_request(async move |req: InitializeRequest, responder, _connection| {
    //!                 responder.respond(
    //!                     InitializeResponse::new(req.protocol_version)
    //!                         .agent_capabilities(AgentCapabilities::new())
    //!                 )
    //!             }, agent_client_protocol_core::on_receive_request!())
    //!             .connect_to(client)
    //!             .await
    //!     }
    //! }
    //!
    //! let agent = MyAgent { name: "my-agent".into() };
    //! ```
    //!
    //! # Important: Don't block the event loop
    //!
    //! Message handlers run on the event loop. Blocking in a handler prevents the
    //! connection from processing new messages. For expensive work:
    //!
    //! - Use [`ConnectionTo::spawn`] to offload work to a background task
    //! - Use [`on_receiving_result`] to schedule work when a response arrives
    //!
    //! [`ConnectTo`]: agent_client_protocol_core::ConnectTo
    //! [`ConnectionTo::spawn`]: agent_client_protocol_core::ConnectionTo::spawn
    //! [`on_receiving_result`]: agent_client_protocol_core::SentRequest::on_receiving_result
    //! [`agent-client-protocol-conductor`]: https://crates.io/crates/agent-client-protocol-conductor
}

pub mod custom_message_handlers {
    //! Pattern: Custom message handlers.
    //!
    //! For reusable message handling logic, implement [`HandleDispatchFrom`] and use
    //! [`MatchDispatch`] or [`MatchDispatchFrom`] for type-safe dispatching.
    //!
    //! This is useful when you need to:
    //! - Share message handling logic across multiple components
    //! - Build complex routing logic that doesn't fit the builder pattern
    //! - Integrate with existing handler infrastructure
    //!
    //! # Example
    //!
    //! ```
    //! use agent_client_protocol_core::{HandleDispatchFrom, Dispatch, Handled, ConnectionTo, UntypedRole};
    //! use agent_client_protocol_core::schema::{InitializeRequest, InitializeResponse, AgentCapabilities};
    //! use agent_client_protocol_core::util::MatchDispatch;
    //!
    //! struct MyHandler;
    //!
    //! impl HandleDispatchFrom<UntypedRole> for MyHandler {
    //!     async fn handle_dispatch_from(
    //!         &mut self,
    //!         message: Dispatch,
    //!         _connection: ConnectionTo<UntypedRole>,
    //!     ) -> Result<Handled<Dispatch>, agent_client_protocol_core::Error> {
    //!         MatchDispatch::new(message)
    //!             .if_request(async |req: InitializeRequest, responder| {
    //!                 responder.respond(
    //!                     InitializeResponse::new(req.protocol_version)
    //!                         .agent_capabilities(AgentCapabilities::new())
    //!                 )
    //!             })
    //!             .await
    //!             .done()
    //!     }
    //!
    //!     fn describe_chain(&self) -> impl std::fmt::Debug {
    //!         "MyHandler"
    //!     }
    //! }
    //! ```
    //!
    //! # When to use `MatchDispatch` vs `MatchDispatchFrom`
    //!
    //! - [`MatchDispatch`] - Use when you don't need peer-aware handling
    //! - [`MatchDispatchFrom`] - Use in proxies where messages come from different
    //!   peers (`Client` vs `Agent`) and may need different handling
    //!
    //! [`HandleDispatchFrom`]: agent_client_protocol_core::HandleDispatchFrom
    //! [`MatchDispatch`]: agent_client_protocol_core::util::MatchDispatch
    //! [`MatchDispatchFrom`]: agent_client_protocol_core::util::MatchDispatchFrom
}

pub mod global_mcp_server {
    //! Pattern: Global MCP server in handler chain.
    //!
    //! Use this pattern when you want a single MCP server that handles tool calls
    //! for all sessions. The server is added to the connection's handler chain and
    //! automatically injects itself into every `NewSessionRequest` that passes through.
    //!
    //! # When to use
    //!
    //! - The MCP server provides stateless tools (no per-session state needed)
    //! - You want the simplest setup with minimal boilerplate
    //! - Tools don't need access to session-specific context
    //!
    //! # Using the builder API
    //!
    //! The simplest way to create an MCP server is with [`McpServer::builder`]:
    //!
    //! ```
    //! use agent_client_protocol_core::mcp_server::McpServer;
    //! use agent_client_protocol_core::{ConnectTo, RunWithConnectionTo, Proxy, Conductor};
    //! use schemars::JsonSchema;
    //! use serde::{Deserialize, Serialize};
    //!
    //! #[derive(Debug, Deserialize, JsonSchema)]
    //! struct EchoParams { message: String }
    //!
    //! #[derive(Debug, Serialize, JsonSchema)]
    //! struct EchoOutput { echoed: String }
    //!
    //! // Build the MCP server with tools
    //! let mcp_server = McpServer::builder("my-tools")
    //!     .tool_fn("echo", "Echoes the input",
    //!         async |params: EchoParams, _cx| {
    //!             Ok(EchoOutput { echoed: params.message })
    //!         },
    //!         agent_client_protocol_core::tool_fn!())
    //!     .build();
    //!
    //! // The proxy component is generic over the MCP server's responder type
    //! struct MyProxy<R> {
    //!     mcp_server: McpServer<Conductor, R>,
    //! }
    //!
    //! impl<R: RunWithConnectionTo<Conductor> + Send + 'static> ConnectTo<Conductor> for MyProxy<R> {
    //!     async fn connect_to(self, conductor: impl ConnectTo<Proxy>) -> Result<(), agent_client_protocol_core::Error> {
    //!         Proxy.builder()
    //!             .with_mcp_server(self.mcp_server)
    //!             .connect_to(conductor)
    //!             .await
    //!     }
    //! }
    //!
    //! let proxy = MyProxy { mcp_server };
    //! ```
    //!
    //! # Using rmcp
    //!
    //! If you have an existing [rmcp](https://docs.rs/rmcp) server implementation,
    //! use [`McpServer::from_rmcp`] from the `agent-client-protocol-rmcp` crate:
    //!
    //! ```
    //! use rmcp::{ServerHandler, tool, tool_router, tool_handler};
    //! use rmcp::handler::server::router::tool::ToolRouter;
    //! use rmcp::handler::server::wrapper::Parameters;
    //! use rmcp::model::*;
    //! use agent_client_protocol_core::mcp_server::McpServer;
    //! use agent_client_protocol_core::Conductor;
    //! use agent_client_protocol_rmcp::McpServerExt;
    //! use serde::{Deserialize, Serialize};
    //!
    //! #[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
    //! struct EchoParams {
    //!     message: String,
    //! }
    //!
    //! #[derive(Clone)]
    //! struct MyMcpServer {
    //!     tool_router: ToolRouter<Self>,
    //! }
    //!
    //! impl MyMcpServer {
    //!     fn new() -> Self {
    //!         Self { tool_router: Self::tool_router() }
    //!     }
    //! }
    //!
    //! #[tool_router]
    //! impl MyMcpServer {
    //!     #[tool(description = "Echoes back the input message")]
    //!     async fn echo(&self, Parameters(params): Parameters<EchoParams>) -> Result<CallToolResult, rmcp::ErrorData> {
    //!         Ok(CallToolResult::success(vec![Content::text(format!("Echo: {}", params.message))]))
    //!     }
    //! }
    //!
    //! #[tool_handler]
    //! impl ServerHandler for MyMcpServer {
    //!     fn get_info(&self) -> ServerInfo {
    //!         ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    //!             .with_protocol_version(ProtocolVersion::V_2024_11_05)
    //!             .with_server_info(Implementation::from_build_env())
    //!     }
    //! }
    //!
    //! // Create an MCP server from the rmcp service
    //! let mcp_server = McpServer::<Conductor, _>::from_rmcp("my-server", MyMcpServer::new);
    //! ```
    //!
    //! The `from_rmcp` function takes a factory closure that creates a new server
    //! instance. This allows each MCP connection to get a fresh server instance.
    //!
    //! # How it works
    //!
    //! When you call [`with_mcp_server`], the MCP server is added as a message
    //! handler. It:
    //!
    //! 1. Intercepts `NewSessionRequest` messages and adds its `acp:UUID` URL to the
    //!    request's `mcp_servers` list
    //! 2. Passes the modified request through to the next handler
    //! 3. Handles incoming MCP protocol messages (tool calls, etc.) for its URL
    //!
    //! [`McpServer::builder`]: agent_client_protocol_core::mcp_server::McpServer::builder
    //! [`McpServer::from_rmcp`]: agent_client_protocol_rmcp::McpServerExt::from_rmcp
    //! [`with_mcp_server`]: agent_client_protocol_core::Builder::with_mcp_server
}

pub mod per_session_mcp_server {
    //! Pattern: Per-session MCP server with workspace context.
    //!
    //! Use this pattern when each session needs its own MCP server instance
    //! with access to session-specific context like the working directory.
    //!
    //! # When to use
    //!
    //! - Tools need access to the session's working directory
    //! - You want to track active sessions or maintain per-session state
    //! - Tools need to customize behavior based on session parameters
    //!
    //! # Basic pattern with `on_proxy_session_start`
    //!
    //! The most common pattern intercepts [`NewSessionRequest`], extracts context,
    //! creates a per-session MCP server, and uses [`on_proxy_session_start`] to
    //! run code after the session is established:
    //!
    //! ```
    //! use agent_client_protocol_core::mcp_server::McpServer;
    //! use agent_client_protocol_core::schema::NewSessionRequest;
    //! use agent_client_protocol_core::{Client, Proxy, Conductor, ConnectTo};
    //!
    //! async fn run_proxy(transport: impl ConnectTo<Proxy>) -> Result<(), agent_client_protocol_core::Error> {
    //!     Proxy.builder()
    //!         .on_receive_request_from(Client, async move |request: NewSessionRequest, responder, connection| {
    //!             // Extract session context from the request
    //!             let workspace_path = request.cwd.clone();
    //!
    //!             // Create tools that capture the workspace path
    //!             let mcp_server = McpServer::builder("workspace-tools")
    //!                 .tool_fn("get_workspace", "Returns the session's workspace directory", {
    //!                     async move |_params: (), _cx| {
    //!                         Ok(workspace_path.display().to_string())
    //!                     }
    //!                 }, agent_client_protocol_core::tool_fn!())
    //!                 .build();
    //!
    //!             // Build the session and run code after it starts
    //!             connection.build_session_from(request)
    //!                 .with_mcp_server(mcp_server)?
    //!                 .on_proxy_session_start(responder, async move |session_id| {
    //!                     // This callback runs after the session-id has been sent to the
    //!                     // client but before any further messages from the client or agent
    //!                     // related to this session have been processed.
    //!                     //
    //!                     // You can use this to store the `session_id` before processing
    //!                     // future messages, or to send a first prompt to the agent before
    //!                     // the client has a chance to do so.
    //!                     tracing::info!(%session_id, "Session started");
    //!                     Ok(())
    //!                 })
    //!         }, agent_client_protocol_core::on_receive_request!())
    //!         .connect_to(transport)
    //!         .await
    //! }
    //! ```
    //!
    //! # How `on_proxy_session_start` works
    //!
    //! [`on_proxy_session_start`] is the non-blocking way to set up a proxy session:
    //!
    //! 1. Sends `NewSessionRequest` to the agent
    //! 2. When the response arrives, responds to the client automatically
    //! 3. Sets up message proxying for the session
    //! 4. Runs your callback with the `SessionId`
    //!
    //! The callback runs after the session is established but doesn't block
    //! the message handler. This is ideal for proxies that just need to inject
    //! tools and track sessions.
    //!
    //! # Alternative: blocking with `start_session_proxy`
    //!
    //! If you need the simpler blocking API (e.g., in a client context where
    //! blocking is safe), use [`block_task`] + [`start_session_proxy`]:
    //!
    //! ```
    //! # use agent_client_protocol_core::mcp_server::McpServer;
    //! # use agent_client_protocol_core::schema::NewSessionRequest;
    //! # use agent_client_protocol_core::{Client, Proxy, Conductor, ConnectTo};
    //! # async fn run_proxy(transport: impl ConnectTo<Proxy>) -> Result<(), agent_client_protocol_core::Error> {
    //!     Proxy.builder()
    //!         .on_receive_request_from(Client, async |request: NewSessionRequest, responder, connection| {
    //!             let cwd = request.cwd.clone();
    //!             let mcp_server = McpServer::builder("tools")
    //!                 .tool_fn("get_cwd", "Returns working directory", {
    //!                     async move |_params: (), _cx| Ok(cwd.display().to_string())
    //!                 }, agent_client_protocol_core::tool_fn!())
    //!                 .build();
    //!
    //!             let session_id = connection.build_session_from(request)
    //!                 .with_mcp_server(mcp_server)?
    //!                 .block_task()
    //!                 .start_session_proxy(responder)
    //!                 .await?;
    //!
    //!             tracing::info!(%session_id, "Session started");
    //!             Ok(())
    //!         }, agent_client_protocol_core::on_receive_request!())
    //!         .connect_to(transport)
    //!         .await
    //! # }
    //! ```
    //!
    //! For patterns where you need to interact with the session before proxying,
    //! use [`start_session`] + [`proxy_remaining_messages`] instead.
    //!
    //! [`start_session`]: agent_client_protocol_core::SessionBuilder::start_session
    //! [`proxy_remaining_messages`]: agent_client_protocol_core::ActiveSession::proxy_remaining_messages
    //!
    //! [`NewSessionRequest`]: agent_client_protocol_core::schema::NewSessionRequest
    //! [`on_proxy_session_start`]: agent_client_protocol_core::SessionBuilder::on_proxy_session_start
    //! [`block_task`]: agent_client_protocol_core::SessionBuilder::block_task
    //! [`start_session_proxy`]: agent_client_protocol_core::SessionBuilder::start_session_proxy
}

pub mod filtering_tools {
    //! Pattern: Filtering which tools are available.
    //!
    //! Use [`disable_tool`] and [`enable_tool`] to control which tools are
    //! visible to clients. This is useful when:
    //!
    //! - Some tools should only be available in certain configurations
    //! - You want to conditionally expose tools based on runtime settings
    //! - You need to restrict access to sensitive tools
    //!
    //! # Disabling specific tools (deny-list)
    //!
    //! By default, all registered tools are enabled. Use [`disable_tool`] to
    //! hide specific tools:
    //!
    //! ```
    //! use agent_client_protocol_core::mcp_server::McpServer;
    //! use agent_client_protocol_core::{Conductor, RunWithConnectionTo};
    //! use schemars::JsonSchema;
    //! use serde::Deserialize;
    //!
    //! #[derive(Debug, Deserialize, JsonSchema)]
    //! struct Params {}
    //!
    //! fn build_server(enable_admin: bool) -> Result<McpServer<Conductor, impl RunWithConnectionTo<Conductor>>, agent_client_protocol_core::Error> {
    //!     let mut builder = McpServer::builder("my-server")
    //!         .tool_fn("echo", "Echo a message",
    //!             async |_p: Params, _cx| Ok("echoed"),
    //!             agent_client_protocol_core::tool_fn!())
    //!         .tool_fn("admin", "Admin-only tool",
    //!             async |_p: Params, _cx| Ok("admin action"),
    //!             agent_client_protocol_core::tool_fn!());
    //!
    //!     // Conditionally disable the admin tool
    //!     if !enable_admin {
    //!         builder = builder.disable_tool("admin")?;
    //!     }
    //!
    //!     Ok(builder.build())
    //! }
    //! ```
    //!
    //! Disabled tools:
    //! - Don't appear in `list_tools` responses
    //! - Return "tool not found" errors if called directly
    //!
    //! # Enabling only specific tools (allow-list)
    //!
    //! Use [`disable_all_tools`] followed by [`enable_tool`] to create an
    //! allow-list where only explicitly enabled tools are available:
    //!
    //! ```
    //! use agent_client_protocol_core::mcp_server::McpServer;
    //! use agent_client_protocol_core::{Conductor, RunWithConnectionTo};
    //! use schemars::JsonSchema;
    //! use serde::Deserialize;
    //!
    //! #[derive(Debug, Deserialize, JsonSchema)]
    //! struct Params {}
    //!
    //! fn build_restricted_server() -> Result<McpServer<Conductor, impl RunWithConnectionTo<Conductor>>, agent_client_protocol_core::Error> {
    //!     McpServer::builder("restricted-server")
    //!         .tool_fn("safe", "Safe operation",
    //!             async |_p: Params, _cx| Ok("safe"),
    //!             agent_client_protocol_core::tool_fn!())
    //!         .tool_fn("dangerous", "Dangerous operation",
    //!             async |_p: Params, _cx| Ok("danger!"),
    //!             agent_client_protocol_core::tool_fn!())
    //!         .tool_fn("experimental", "Experimental feature",
    //!             async |_p: Params, _cx| Ok("experimental"),
    //!             agent_client_protocol_core::tool_fn!())
    //!         // Start with all tools disabled
    //!         .disable_all_tools()
    //!         // Only enable the safe tool
    //!         .enable_tool("safe")
    //!         .map(|b| b.build())
    //! }
    //! ```
    //!
    //! # Error handling
    //!
    //! Both [`enable_tool`] and [`disable_tool`] return `Result` and will error
    //! if the tool name doesn't match any registered tool. This helps catch typos:
    //!
    //! ```
    //! use agent_client_protocol_core::mcp_server::McpServer;
    //! use agent_client_protocol_core::Conductor;
    //!
    //! // This will error because "ech" is not a registered tool
    //! let result = McpServer::<Conductor, _>::builder("server")
    //!     .disable_tool("ech");  // Typo! Should be "echo"
    //!
    //! assert!(result.is_err());
    //! ```
    //!
    //! Calling enable/disable on an already enabled/disabled tool is not an error -
    //! the operations are idempotent.
    //!
    //! [`disable_tool`]: agent_client_protocol_core::mcp_server::McpServerBuilder::disable_tool
    //! [`enable_tool`]: agent_client_protocol_core::mcp_server::McpServerBuilder::enable_tool
    //! [`disable_all_tools`]: agent_client_protocol_core::mcp_server::McpServerBuilder::disable_all_tools
}

pub mod running_proxies_with_conductor {
    //! Pattern: Running proxies with the conductor.
    //!
    //! Proxies don't run standalone. To add an MCP server (or other proxy behavior)
    //! to an existing agent, you need the **conductor** to orchestrate the connection.
    //!
    //! The conductor:
    //! 1. Accepts connections from clients
    //! 2. Chains your proxies together
    //! 3. Connects to the final agent
    //! 4. Routes messages through the entire chain
    //!
    //! # Using the `agent-client-protocol-conductor` binary
    //!
    //! The simplest way to run a proxy is with the [`agent-client-protocol-conductor`] binary.
    //! Configure it with a JSON file:
    //!
    //! ```json
    //! {
    //!   "proxies": [
    //!     { "command": ["cargo", "run", "--bin", "my-proxy"] }
    //!   ],
    //!   "agent": { "command": ["claude-code", "--agent"] }
    //! }
    //! ```
    //!
    //! Then run:
    //!
    //! ```bash
    //! agent-client-protocol-conductor --config conductor.json
    //! ```
    //!
    //! # Using the conductor as a library
    //!
    //! For more control, use [`agent-client-protocol-conductor`] as a library with the `ConductorImpl` type:
    //!
    //! ```ignore
    //! use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
    //!
    //! // Define your proxy as a ConnectTo<Conductor>
    //! let my_proxy = MyProxy::new();
    //!
    //! // Spawn the agent process
    //! let agent_process = agent_client_protocol_tokio::spawn_process("claude-code", &["--agent"]).await?;
    //!
    //! // Create the conductor with your proxy chain
    //! let conductor = ConductorImpl::new(ProxiesAndAgent {
    //!     proxies: vec![Box::new(my_proxy)],
    //!     agent: agent_process,
    //! });
    //!
    //! // Run the conductor (it will accept client connections on stdin/stdout)
    //! conductor.connect_to(client_transport).await?;
    //! ```
    //!
    //! # Why can't I just connect my proxy directly to an agent?
    //!
    //! ACP uses a message envelope format for proxy chains. When a proxy sends a
    //! message toward the agent, it gets wrapped in a [`SuccessorMessage`] envelope.
    //! The conductor handles this wrapping/unwrapping automatically.
    //!
    //! If you connected directly to an agent, your proxy would send `SuccessorMessage`
    //! envelopes that the agent doesn't understand.
    //!
    //! # Example: Complete proxy with conductor
    //!
    //! See the [`agent-client-protocol-conductor` tests] for complete working examples of proxies
    //! running with the conductor.
    //!
    //! [`agent-client-protocol-conductor`]: https://crates.io/crates/agent-client-protocol-conductor
    //! [`SuccessorMessage`]: agent_client_protocol_core::schema::SuccessorMessage
    //! [`agent-client-protocol-conductor` tests]: https://github.com/anthropics/acp-rust-sdk/tree/main/src/agent-client-protocol-conductor/tests
}
