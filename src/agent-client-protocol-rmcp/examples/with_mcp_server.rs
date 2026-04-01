//! Proxy with MCP server example
//!
//! This proxy provides a simple MCP server with an "echo" tool.
//! Demonstrates how to add MCP tools to any agent through a proxy.
//!
//! Run with:
//! ```bash
//! cargo run --example with_mcp_server
//! ```

use agent_client_protocol_core::{Proxy, mcp_server::McpServer};
use agent_client_protocol_rmcp::McpServerExt;
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Parameters for the echo tool
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct EchoParams {
    /// The message to echo back
    message: String,
}

/// Simple MCP server with an echo tool
#[derive(Clone, Debug)]
pub struct ExampleMcpServer {
    tool_router: ToolRouter<ExampleMcpServer>,
}

impl ExampleMcpServer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

impl Default for ExampleMcpServer {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router]
impl ExampleMcpServer {
    /// Echo tool - returns the input message
    #[tool(description = "Echoes back the input message")]
    async fn echo(
        &self,
        Parameters(params): Parameters<EchoParams>,
    ) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Echo: {}",
            params.message
        ))]))
    }
}

#[tool_handler]
impl ServerHandler for ExampleMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("example-mcp-server", "0.1.0"))
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions("A simple example MCP server with an echo tool")
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing for debugging
    tracing_subscriber::fmt()
        .with_target(true)
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("MCP server proxy starting");

    // Create an MCP server from the rmcp service
    let mcp_server = McpServer::from_rmcp("example", ExampleMcpServer::new);

    // Set up the proxy connection with our MCP server
    // ProxyToConductor already has proxy behavior built into its default_message_handler
    let proxy = Proxy
        .builder()
        .name("mcp-server-proxy")
        // Register the MCP server as a handler
        .with_mcp_server(mcp_server)
        // Start serving
        .connect_to(agent_client_protocol_core::ByteStreams::new(
            tokio::io::stdout().compat_write(),
            tokio::io::stdin().compat(),
        ));
    Box::pin(proxy).await?;

    Ok(())
}
