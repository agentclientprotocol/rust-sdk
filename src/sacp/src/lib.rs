#![deny(missing_docs)]

//! # sacp -- the Symposium Agent Client Protocol (ACP) SDK
//!
//! **sacp** is a Rust SDK for building agents and editors using the [Agent-Client Protocol (ACP)](https://agentclientprotocol.com/).
//! It makes it easy to build ACP editors and clients -- or, indeed, any JSON-RPC-based application.
//!
//! ## Quick Start
//!
//! Building an ACP agent is straightforward with sacp's type-safe API:
//!
//! ```no_run
//! use sacp::{JrHandlerChain, MessageAndCx, UntypedMessage};
//! use sacp::schema::{InitializeRequest, InitializeResponse, AgentCapabilities};
//! use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), sacp::Error> {
//! // Start by creating an agent connection
//! JrHandlerChain::new()
//! .name("my-agent") // Give it a name for logging purposes
//! .on_receive_request(async move |initialize: InitializeRequest, request_cx| {
//!     // Create one or more request handlers -- these are attempted in order.
//!     // You can do anything you want in here, but you should eventually
//!     // respond to the request with `request_cx.respond(...)`:
//!     request_cx.respond(InitializeResponse {
//!         protocol_version: initialize.protocol_version,
//!         agent_capabilities: AgentCapabilities::default(),
//!         auth_methods: Default::default(),
//!         agent_info: Default::default(),
//!         meta: Default::default(),
//!     })
//! })
//! .on_receive_message(async move |message: MessageAndCx<UntypedMessage, UntypedMessage>| {
//!     // You can also handle any kind of message:
//!     message.respond_with_error(sacp::util::internal_error("TODO"))
//! })
//! .serve(sacp::ByteStreams::new(
//!     tokio::io::stdout().compat_write(),
//!     tokio::io::stdin().compat(),
//! ))
//! .await
//! # }
//! ```
//!
//! ## Common Patterns
//!
//! ### Pattern 1: Defining Reusable Components
//!
//! When building agents or proxies, define a struct that implements [`Component`]. Internally, use [`JrHandlerChain`] to set up handlers:
//!
//! ```rust,ignore
//! use sacp::{Component, JrHandlerChain};
//!
//! struct MyAgent {
//!     config: AgentConfig,
//! }
//!
//! impl Component for MyAgent {
//!     async fn serve(self, client: impl Component) -> Result<(), sacp::Error> {
//!         JrHandlerChain::new()
//!             .name("my-agent")
//!             .on_receive_request(async move |req: PromptRequest, cx| {
//!                 // Don't block the message loop! Use await_when_* for async work
//!                 cx.respond(self.process_prompt(req))
//!                     .await_when_result_received(async move |response| {
//!                         // This runs after the response is received
//!                         log_response(&response);
//!                         cx.respond(response)
//!                     })
//!             })
//!             .serve(client)
//!             .await
//!     }
//! }
//! ```
//!
//! **Important:** Message handlers run on the event loop. Blocking in a handler will prevent the connection from processing new messages.
//! Use [`JrConnectionCx::spawn`] to offload expensive work, or use the `await_when_*` methods to avoid blocking.
//!
//! ### Pattern 2: Custom Message Handlers
//!
//! For reusable message handling logic, implement [`JrMessageHandler`] and use [`MatchMessage`](crate::util::MatchMessage) for dispatching:
//!
//! ```rust,ignore
//! use sacp::{JrMessageHandler, MessageAndCx, Handled};
//! use sacp::util::MatchMessage;
//!
//! struct MyHandler {
//!     state: Arc<Mutex<State>>,
//! }
//!
//! impl JrMessageHandler for MyHandler {
//!     async fn handle_message(&mut self, message: MessageAndCx)
//!         -> Result<Handled<MessageAndCx>, sacp::Error>
//!     {
//!         MatchMessage::new(message)
//!             .if_request(async |req: MyRequest, cx| {
//!                 // Handle using self.state
//!                 cx.respond(MyResponse { /* ... */ })
//!             })
//!             .await
//!             .done()
//!     }
//!
//!     fn describe_chain(&self) -> impl std::fmt::Debug { "MyHandler" }
//! }
//! ```
//!
//! ### Pattern 3: Connecting as a Client
//!
//! To connect to a JSON-RPC server and send requests, use `with_client`. Note the use of `async` (not `async move`)
//! to share access to local variables:
//!
//! ```rust,ignore
//! JrHandlerChain::new()
//!     .on_receive_notification(async |notif: SessionUpdate, cx| {
//!         // Handle notifications from the server
//!         Ok(())
//!     })
//!     .with_client(sacp::ByteStreams::new(stdout, stdin), async |cx| {
//!         // Send requests using the connection context
//!         let response = cx.send_request(MyRequest { /* ... */ })
//!             .block_task()
//!             .await?;
//!
//!         // Can access local variables here
//!         process_response(response);
//!
//!         Ok(())
//!     })
//!     .await
//! ```
//!
//! ## Using the Request Context
//!
//! The request context ([`JrRequestCx`]) provided to handlers is not just for responding—it offers several capabilities:
//!
//! - **Respond to the request:** `cx.respond(response)` sends a response back to the caller
//! - **Send requests to the other side:** `cx.send_request(request)` initiates a new request
//! - **Send notifications:** `cx.send_notification(notification)` sends a fire-and-forget message
//! - **Spawn background tasks:** `cx.spawn(future)` runs work concurrently without blocking the message loop
//!
//! If you need a connection context that's independent of a particular request (for example, to store in a struct for later use),
//! use `cx.connection_cx()`. This gives you a [`JrConnectionCx`] that can spawn tasks and send messages but isn't tied to
//! responding to a specific request.
//!
//! ## Learning more
//!
//! You can learn more in the [docs for `JrConnection`](crate::JrConnection) or on our
//! [GitHub Pages](https://github.com/symposium-acp/symposium-acp) site.
//!
//! You may also enjoy looking at some of these examples:
//!
//! - **[`simple_agent.rs`](https://github.com/symposium-org/symposium-acp/blob/main/src/sacp/examples/simple_agent.rs)** - Minimal agent implementation
//! - **[`yolo_one_shot_client.rs`](https://github.com/symposium-org/symposium-acp/blob/main/src/sacp/examples/yolo_one_shot_client.rs)** - Complete client that spawns an agent and sends a prompt
//! - **[`elizacp`](https://crates.io/crates/elizacp)** - Full working agent with session management (also useful for testing)
//! - **[`sacp-conductor`](https://crates.io/crates/sacp-conductor)** - The "conductor" is an ACP agent that composes proxy components with a final agent.
//!
//! ## Related Crates
//!
//! - **[sacp-proxy](https://crates.io/crates/sacp-proxy)** - Framework for building ACP proxies that extend agent behavior
//! - **[sacp-tokio](https://crates.io/crates/sacp-tokio)** - Tokio-specific utilities (process spawning, connection management)
//! - **[sacp-conductor](https://crates.io/crates/sacp-conductor)** - Binary for orchestrating proxy chains

/// Capability management for the `_meta.symposium` object
mod capabilities;
/// Component abstraction for agents and proxies
pub mod component;
/// JSON-RPC handler types for building custom message handlers
pub mod handler;
/// JSON-RPC connection and handler infrastructure
mod jsonrpc;
/// ACP protocol schema types - all message types, requests, responses, and supporting types
pub mod schema;
/// Utility functions and types
pub mod util;

pub use capabilities::*;

/// JSON-RPC message types.
///
/// This module re-exports types from the `jsonrpcmsg` crate that are transitively
/// reachable through the public API (e.g., via [`Channel`]).
///
/// Users of the `sacp` crate can use these types without adding a direct dependency
/// on `jsonrpcmsg`.
pub mod jsonrpcmsg {
    pub use jsonrpcmsg::{Id, Message, Params, Request, Response};
}

pub use jsonrpc::{
    ByteStreams, Channel, Handled, IntoHandled, JrConnection, JrConnectionCx, JrHandlerChain,
    JrMessage, JrMessageHandler, JrNotification, JrRequest, JrRequestCx, JrResponse,
    JrResponsePayload, MessageAndCx, UntypedMessage,
};

pub use component::{Component, DynComponent};

// Re-export BoxFuture for implementing Component traits
pub use futures::future::BoxFuture;

// Re-export the six primary message enum types at the root
pub use schema::{
    AgentNotification, AgentRequest, AgentResponse, ClientNotification, ClientRequest,
    ClientResponse,
};

// Re-export commonly used infrastructure types for convenience
pub use schema::{Error, ErrorCode};
