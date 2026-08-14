//! Creating and managing sessions for multi-turn conversations.
//!
//! A **session** represents a multi-turn conversation with an agent. Within a
//! session, you can send prompts, receive responses, and the agent maintains
//! context across turns.
//!
//! The examples below use the stable protocol v1 `SessionBuilder` and
//! `ActiveSession`. With the `unstable_protocol_v2` feature, callbacks created
//! through `Client.v2()` receive `V2ConnectionTo` and its `build_session*`,
//! `V2SessionBuilder`, `resume_session*`, `V2ResumeSessionBuilder`, and
//! command-only `V2Session` APIs. The v2 resume helpers return a builder and do
//! not publish `session/resume` until `start_session` or
//! `on_proxy_session_start` is called. V2 prompt responses acknowledge
//! acceptance independently; receive session-wide updates and interactive
//! requests through typed connection handlers.
//!
//! # Creating a Session
//!
//! Use the session builder to create a new session:
//!
//! ```
//! # use agent_client_protocol::{Client, Agent, ConnectTo};
//! # async fn example(transport: impl ConnectTo<Client>) -> Result<(), agent_client_protocol::Error> {
//! # Client.builder().connect_with(transport, async |cx| {
//! cx.build_session_cwd()?          // Use current working directory
//!     .block_task()                // Mark as blocking
//!     .run_until(async |session| {
//!         // Use the session here
//!         Ok(())
//!     })
//!     .await?;
//! # Ok(())
//! # }).await?;
//! # Ok(())
//! # }
//! ```
//!
//! Or specify a custom working directory:
//!
//! ```
//! # use agent_client_protocol::{Client, Agent, ConnectTo};
//! # async fn example(transport: impl ConnectTo<Client>) -> Result<(), agent_client_protocol::Error> {
//! # Client.builder().connect_with(transport, async |cx| {
//! cx.build_session("/path/to/project")
//!     .block_task()
//!     .run_until(async |session| { Ok(()) })
//!     .await?;
//! # Ok(())
//! # }).await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Sending Prompts
//!
//! Inside `run_until`, you get an [`ActiveSession`] that lets you interact
//! with the agent:
//!
//! ```
//! # use agent_client_protocol::{Client, Agent, ConnectTo};
//! # async fn example(transport: impl ConnectTo<Client>) -> Result<(), agent_client_protocol::Error> {
//! # Client.builder().connect_with(transport, async |cx| {
//! # cx.build_session_cwd()?.block_task()
//! .run_until(async |mut session| {
//!     // Send a prompt
//!     session.send_prompt("What is 2 + 2?")?;
//!
//!     // Read the complete response as a string
//!     let response = session.read_to_string().await?;
//!     println!("{}", response);
//!
//!     // Send another prompt in the same session
//!     session.send_prompt("And what is 3 + 3?")?;
//!     let response = session.read_to_string().await?;
//!
//!     Ok(())
//! })
//! # .await?;
//! # Ok(())
//! # }).await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Adding MCP Servers
//!
//! You can attach MCP (Model Context Protocol) servers to a session to provide
//! tools to the agent:
//!
//! MCP attachment requires the `unstable_mcp_over_acp` feature. Standalone MCP
//! servers remain available without it. Draft protocol v2 per-session
//! attachment uses `V2SessionBuilder::with_mcp_server` for new sessions or
//! `V2ResumeSessionBuilder::with_mcp_server` for resumed sessions and
//! additionally requires `unstable_protocol_v2`. The SDK installs the routes
//! and initially polls the runners before publishing the setup request, so the
//! agent can use them during setup or resume replay. Successful attachments
//! remain active for the connection lifetime; setup failures, including an
//! error response after cancellation, clean up the pending attachment.
//!
//! ```ignore
//! # use agent_client_protocol::{Client, Agent, ConnectTo};
//! # use agent_client_protocol::mcp_server::McpServer;
//! # use agent_client_protocol_rmcp::McpServerExt;
//! # async fn example(transport: impl ConnectTo<Client>) -> Result<(), agent_client_protocol::Error> {
//! # let my_mcp_server = McpServer::<Agent, _>::builder("tools").build();
//! # Client.builder().connect_with(transport, async |cx| {
//! cx.build_session_cwd()?
//!     .with_mcp_server(my_mcp_server)?
//!     .block_task()
//!     .run_until(async |session| { Ok(()) })
//!     .await?;
//! # Ok(())
//! # }).await?;
//! # Ok(())
//! # }
//! ```
//!
//! See the cookbook for detailed MCP server examples.
//!
//! # Non-Blocking Session Start
//!
//! If you're inside an `on_receive_*` callback and need to start a session,
//! use `on_session_start` instead of `block_task().run_until()`:
//!
//! ```
//! # use agent_client_protocol::{Client, Agent, ConnectTo};
//! # use agent_client_protocol::schema::v1::NewSessionRequest;
//! # async fn example(transport: impl ConnectTo<Client>) -> Result<(), agent_client_protocol::Error> {
//! Client.builder()
//!     .on_receive_request(async |req: NewSessionRequest, responder, cx| {
//!         cx.build_session_from(req)
//!             .on_session_start(async |session| {
//!                 // Handle the session
//!                 Ok(())
//!             })?;
//!         Ok(())
//!     }, agent_client_protocol::on_receive_request!())
//! #   .connect_with(transport, async |_| Ok(())).await?;
//! # Ok(())
//! # }
//! ```
//!
//! When the session response is routed during its original dispatch, session
//! routing is installed before later messages are dispatched. The callback is
//! invoked in a spawned task, so no user callback code has that ordering
//! guarantee and the callback can wait for session traffic. A response
//! interceptor that retains and routes the response later cannot retroactively
//! order setup before messages already processed. See [Ordering](super::ordering)
//! for details.
//!
//! For a draft v2 proxy, use `V2SessionBuilder::on_proxy_session_start` or
//! `V2ResumeSessionBuilder::on_proxy_session_start` instead. Each forwards the
//! complete operation-specific response and then spawns the callback with an
//! `OpenedV2Session`, so the callback keeps both the command-only session handle
//! and that exact response:
//!
//! ```rust,ignore
//! Proxy.v2()
//!     .on_receive_request_from(
//!         Client,
//!         async |request: schema::v2::NewSessionRequest, responder, cx| {
//!             cx.build_session_from(request)
//!                 .on_proxy_session_start(responder, async |opened| {
//!                     let (session, setup_response) = opened.into_parts();
//!                     track_session(session.session_id(), setup_response);
//!                     Ok(())
//!                 })
//!         },
//!         agent_client_protocol::on_receive_request!(),
//!     );
//! ```
//!
//! For `session/resume`, the builder installs and acknowledges session routing
//! before publishing the downstream request, allowing replay updates to be
//! forwarded before the resume response. The downstream request inherits
//! upstream cancellation. An unsuccessful downstream response drops pending
//! routing and MCP attachment; successful setup keeps those routes for the
//! connection lifetime. The cancellation signal itself remains advisory while
//! the helper awaits that response. User work runs outside the ordering
//! barrier. V2 session updates and interactive requests remain independent
//! traffic handled by typed connection callbacks.
//!
//! # Next Steps
//!
//! - [Callbacks](super::callbacks) - Handle incoming requests
//! - [Ordering](super::ordering) - Understand when to use `block_task` vs `on_*`
//!
//! [`ActiveSession`]: crate::ActiveSession
