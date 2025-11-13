#![deny(missing_docs)]

//! # sacp-proxy
//!
//! Framework for building ACP proxy components that extend agent functionality.
//!
//! ## What are proxies?
//!
//! Proxies are modular components that sit between an editor and an agent, intercepting and transforming messages.
//! They enable **composable agent architectures** where functionality can be added without modifying the base agent.
//!
//! ```text
//! Editor → Proxy 1 → Proxy 2 → Agent
//! ```
//!
//! Use cases:
//! - Add MCP tools/resources/prompts to any agent
//! - Inject context or modify prompts before they reach the agent
//! - Filter or transform agent responses
//! - Add logging, metrics, or policy enforcement
//!
//! ## Quick Start
//!
//! The simplest proxy just forwards messages unchanged:
//!
//! ```rust,no_run
//! use sacp::JrHandlerChain;
//! use sacp_proxy::{AcpProxyExt, McpServiceRegistry};
//! use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! JrHandlerChain::new()
//!     .name("my-proxy")
//!     .provide_mcp(McpServiceRegistry::default())  // Provide MCP services
//!     .proxy()  // Enable proxy mode
//!     .serve(sacp::ByteStreams::new(
//!         tokio::io::stdout().compat_write(),
//!         tokio::io::stdin().compat()
//!     ))
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! To add MCP tools to the proxy, provide an MCP server:
//!
//! ```rust,no_run
//! use sacp::JrHandlerChain;
//! use sacp_proxy::{AcpProxyExt, McpServiceRegistry};
//! use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! JrHandlerChain::new()
//!     .name("my-proxy")
//!     // Add MCP servers to provide tools/resources/prompts
//!     .provide_mcp(
//!         McpServiceRegistry::default()
//!             // .with_rmcp_server("my-server", || MyMcpServer::new())?
//!     )
//!     .proxy()
//!     .serve(sacp::ByteStreams::new(
//!         tokio::io::stdout().compat_write(),
//!         tokio::io::stdin().compat()
//!     ))
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! See the `with_mcp_server.rs` example for a complete implementation.
//!
//! ## Key Concepts
//!
//! **Predecessor vs Successor:**
//! - **Predecessor** - The component closer to the editor (could be editor or another proxy)
//! - **Successor** - The component farther from the editor (could be agent or another proxy)
//!
//! Messages flow: `Predecessor → This Proxy → Successor`
//!
//! **Extension Traits:**
//! - [`AcpProxyExt`] - Adds proxy-specific methods to [`JrConnection`](sacp::JrConnection)
//! - [`JrCxExt`] - Adds forwarding methods (`send_to_predecessor`, `send_to_successor`)
//!
//! ## Examples
//!
//! See the [examples directory](https://github.com/symposium-org/symposium-acp/tree/main/src/sacp-proxy/examples):
//!
//! - **[`minimal.rs`](https://github.com/symposium-org/symposium-acp/blob/main/src/sacp-proxy/examples/minimal.rs)** - Simplest possible proxy that forwards everything unchanged
//! - **[`with_mcp_server.rs`](https://github.com/symposium-org/symposium-acp/blob/main/src/sacp-proxy/examples/with_mcp_server.rs)** - Proxy that adds MCP tools to any agent
//!
//! ## How Proxies Work
//!
//! When you call `.proxy()`, the framework:
//! 1. Handles capability negotiation with predecessor and successor
//! 2. Sets up message routing between components
//! 3. Wraps your handlers to intercept specific message types
//! 4. Forwards everything else automatically
//!
//! You only need to handle the messages you want to intercept or transform.
//!
//! ## Related Crates
//!
//! - **[sacp](https://crates.io/crates/sacp)** - Core ACP SDK
//! - **[sacp-conductor](https://crates.io/crates/sacp-conductor)** - Binary for orchestrating proxy chains
//! - **[sacp-tokio](https://crates.io/crates/sacp-tokio)** - Tokio utilities for spawning agents

/// Re-export component types from sacp
pub use sacp::component::Component;

/// Proxying MCP messages over ACP.
mod mcp_over_acp;
pub use mcp_over_acp::*;

/// Proxying and sending messages to/from the successor component
mod to_from_successor;
pub use to_from_successor::*;

mod mcp_server;
pub use mcp_server::*;
