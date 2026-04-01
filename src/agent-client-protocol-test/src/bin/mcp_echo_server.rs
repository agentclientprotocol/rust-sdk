//! Simple MCP echo server for testing
//!
//! This server provides a basic "echo" tool that returns the input message.
//! It runs over stdio and can be used for testing MCP integrations.

use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};

/// Parameters for the echo tool
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct EchoParams {
    /// The message to echo back
    message: String,
}

/// Simple MCP server with an echo tool
#[derive(Clone)]
struct EchoServer {
    tool_router: ToolRouter<EchoServer>,
}

impl EchoServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl EchoServer {
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
impl ServerHandler for EchoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("mcp-echo-server", "1.0.0"))
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions("A simple MCP server with an echo tool for testing")
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("Starting MCP echo server");

    // Run the server on stdio
    let service = EchoServer::new().serve(rmcp::transport::stdio()).await?;

    // Keep the service running indefinitely
    service.waiting().await?;

    Ok(())
}
