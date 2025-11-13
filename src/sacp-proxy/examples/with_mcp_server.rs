//! Proxy with MCP server example
//!
//! This proxy provides a simple MCP server with an "echo" tool.
//! Demonstrates how to add MCP tools to any agent through a proxy.
//!
//! Run with:
//! ```bash
//! cargo run --example with_mcp_server
//! ```

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
};
use sacp::JrHandlerChain;
use sacp_proxy::{AcpProxyExt, McpServiceRegistry};
use serde::{Deserialize, Serialize};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Parameters for the echo tool
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct EchoParams {
    /// The message to echo back
    message: String,
}

/// Simple MCP server with an echo tool
#[derive(Clone)]
pub struct ExampleMcpServer {
    tool_router: ToolRouter<ExampleMcpServer>,
}

impl ExampleMcpServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
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
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "example-mcp-server".to_string(),
                version: "0.1.0".to_string(),
                icons: None,
                title: None,
                website_url: None,
            },
            instructions: Some("A simple example MCP server with an echo tool".to_string()),
        }
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

    // Set up the proxy connection with our MCP server
    JrHandlerChain::new()
        .name("mcp-server-proxy")
        // Register the MCP server named "example"
        .provide_mcp(
            McpServiceRegistry::default().with_rmcp_server("example", ExampleMcpServer::new)?,
        )
        // Enable proxy mode
        .proxy()
        // Start serving
        .connect_to(sacp::ByteStreams::new(
            tokio::io::stdout().compat_write(),
            tokio::io::stdin().compat(),
        ))?
        .serve()
        .await?;

    Ok(())
}
