//! Building proxies that intercept and modify messages.
//!
//! A **proxy** sits between a client and an agent, intercepting messages
//! in both directions. This is how you add capabilities like MCP tools,
//! logging, or message transformation.
//!
//! # The Proxy Role Type
//!
//! Proxies use the [`Proxy`] role type, which has two peers:
//!
//! - [`Client`] - messages from/to the client direction
//! - [`Agent`] - messages from/to the agent direction
//!
//! Unlike simpler links, there's no default peer - you must always specify
//! which direction you're communicating with.
//!
//! # Choosing a Protocol Version
//!
//! `Proxy::builder` creates a stable protocol v1 proxy. With the
//! `unstable_protocol_v2` feature, `Proxy.v2()` creates a v2-only proxy whose
//! fluent callbacks receive `V2ConnectionTo<Conductor>`. The builder
//! validates `_proxy/initialize` and later traffic against the selected
//! version.
//!
//! Use `Proxy.protocol_router()` to package separate v1 and v2 implementations
//! behind one `ConnectTo<Conductor>` component. It dispatches the
//! conductor-selected `_proxy/initialize` version exactly and performs no
//! cross-version conversion.
//!
//! Low-level infrastructure implementing custom raw version routing can use
//! `Proxy.builder().without_acp_version_guard()`. Disabling the guard is an
//! explicit version-neutral escape hatch, not the ordinary way to author a v2
//! proxy.
//!
//! # Default Forwarding
//!
//! By default, [`Proxy`] forwards all messages it doesn't handle.
//! This means a minimal stable v1 proxy that does nothing is just:
//!
//! ```
//! # use agent_client_protocol::{Proxy, Conductor, ConnectTo};
//! # async fn example(transport: impl ConnectTo<Proxy>) -> Result<(), agent_client_protocol::Error> {
//! Proxy.builder()
//!     .connect_to(transport)
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! All messages pass through unchanged.
//!
//! # Intercepting Messages
//!
//! To intercept specific messages, use `on_receive_*_from` with explicit peers:
//!
//! ```
//! # use agent_client_protocol::{Proxy, Client, Agent, Conductor, ConnectTo};
//! # use agent_client_protocol_test::ProcessRequest;
//! # async fn example(transport: impl ConnectTo<Proxy>) -> Result<(), agent_client_protocol::Error> {
//! Proxy.builder()
//!     // Intercept requests from the client
//!     .on_receive_request_from(Client, async |req: ProcessRequest, responder, cx| {
//!         // Modify the request
//!         let modified = ProcessRequest {
//!             data: format!("prefix: {}", req.data),
//!         };
//!
//!         // Forward to agent and relay the response back
//!         cx.send_request_to(Agent, modified)
//!             .forward_response_to(responder)
//!     }, agent_client_protocol::on_receive_request!())
//!     .connect_to(transport)
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! Messages you don't handle are forwarded automatically.
//!
//! # Adding MCP Servers
//!
//! A common use case is adding tools via MCP. You can add them globally
//! (available in all sessions) or per-session.
//!
//! These ACP attachment APIs require the `unstable_mcp_over_acp` feature.
//! Draft v2 attachment additionally requires `unstable_protocol_v2`.
//!
//! ## Global MCP Server
//!
//! ```ignore
//! # use agent_client_protocol::{Proxy, Conductor, ConnectTo};
//! # use agent_client_protocol::mcp_server::McpServer;
//! # use agent_client_protocol_rmcp::McpServerExt;
//! # async fn example(transport: impl ConnectTo<Proxy>) -> Result<(), agent_client_protocol::Error> {
//! # let my_mcp_server = McpServer::<Conductor, _>::builder("tools").build();
//! Proxy.builder()
//!     .with_mcp_server(my_mcp_server)
//!     .connect_to(transport)
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! For draft v2, select the v2 proxy builder before attaching the global
//! server:
//!
//! ```rust,ignore
//! Proxy.v2()
//!     .with_mcp_server(my_mcp_server)
//!     .connect_to(transport)
//!     .await?;
//! ```
//!
//! The v1 builder injects the declaration into new, load, resume, and
//! feature-gated fork requests. The v2 builder injects it into new, resume,
//! and feature-gated fork requests while preserving unrelated setup fields.
//! Both reuse one connection-scoped server ID.
//!
//! ## Per-Session MCP Server
//!
//! ```ignore
//! # use agent_client_protocol::{Proxy, Client, Conductor, ConnectTo};
//! # use agent_client_protocol::schema::v1::NewSessionRequest;
//! # use agent_client_protocol::mcp_server::McpServer;
//! # use agent_client_protocol_rmcp::McpServerExt;
//! # async fn example(transport: impl ConnectTo<Proxy>) -> Result<(), agent_client_protocol::Error> {
//! Proxy.builder()
//!     .on_receive_request_from(Client, async |req: NewSessionRequest, responder, cx| {
//!         let my_mcp_server = McpServer::<Conductor, _>::builder("tools").build();
//!         cx.build_session_from(req)
//!             .with_mcp_server(my_mcp_server)?
//!             .on_proxy_session_start(responder, async |session_id| {
//!                 // Session started with MCP server attached
//!                 Ok(())
//!             })
//!     }, agent_client_protocol::on_receive_request!())
//!     .connect_to(transport)
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! The corresponding v2 proxy uses `Proxy.v2()`, a
//! `schema::v2::NewSessionRequest`, and the same fluent session-builder shape.
//! Its `V2SessionBuilder::on_proxy_session_start` callback receives an
//! `OpenedV2Session` containing both the command-only v2 session handle and the
//! complete `NewSessionResponse`:
//!
//! ```rust,ignore
//! Proxy.v2()
//!     .on_receive_request_from(
//!         Client,
//!         async |request: schema::v2::NewSessionRequest, responder, cx| {
//!             cx.build_session_from(request)
//!                 .with_mcp_server(my_mcp_server)?
//!                 .on_proxy_session_start(responder, async |opened| {
//!                     let (session, response) = opened.into_parts();
//!                     track_session(session.session_id(), response);
//!                     Ok(())
//!                 })
//!         },
//!         agent_client_protocol::on_receive_request!(),
//!     );
//! ```
//!
//! For `schema::v2::ResumeSessionRequest`, use
//! `cx.resume_session_from(request)` and the resulting
//! `V2ResumeSessionBuilder` in the same shape. With
//! `unstable_session_fork`, use `cx.fork_session_from(request)` and
//! `V2ForkSessionBuilder` for `ForkSessionRequest`; the returned session and
//! installed route use the new ID from `ForkSessionResponse`, not the source
//! session ID. Resume routing and any per-session MCP attachment are ready
//! before the downstream request is published, allowing replay to precede the
//! complete response. All setup helpers forward that operation's response
//! before spawning the callback. Later updates and interactive requests remain
//! independent traffic handled by typed connection callbacks.
//!
//! # The Conductor
//!
//! Proxies don't run standalone - they're orchestrated by a **conductor**.
//! The conductor:
//!
//! - Spawns proxy processes
//! - Chains them together
//! - Connects the final proxy to the agent
//!
//! The [`agent-client-protocol-conductor`] crate provides a conductor binary. You configure
//! it with a list of proxies to run.
//!
//! # Proxy Chains
//!
//! Multiple proxies can be chained:
//!
//! ```text
//! Client <-> Proxy A <-> Proxy B <-> Agent
//! ```
//!
//! Each proxy sees messages from its perspective:
//! - `Client` is "toward the client" (Proxy A, or conductor if first)
//! - `Agent` is "toward the agent" (Proxy B, or agent if last)
//!
//! Messages flow through each proxy in order. Each can inspect, modify,
//! or handle messages before they continue.
//!
//! # Summary
//!
//! | Task | Approach |
//! |------|----------|
//! | Forward everything | Just `connect_to(transport)` |
//! | Author a v1 or v2 proxy | `Proxy.builder()` or `Proxy.v2()` |
//! | Package v1 and v2 implementations | `Proxy.protocol_router().with_v1(...).with_v2(...)` |
//! | Implement custom raw routing | `without_acp_version_guard` on the raw proxy builder |
//! | Intercept specific messages | `on_receive_*_from` with explicit peers |
//! | Add global tools | `with_mcp_server` on builder |
//! | Add per-session tools | `with_mcp_server` on session builder |
//!
//! [`Proxy`]: crate::Proxy
//! [`Client`]: crate::Client
//! [`Agent`]: crate::Agent
//! [`agent-client-protocol-conductor`]: https://crates.io/crates/agent-client-protocol-conductor
