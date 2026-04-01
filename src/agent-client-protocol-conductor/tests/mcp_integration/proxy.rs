//! Proxy component that provides MCP tools

use agent_client_protocol_core::mcp_server::McpServer;
use agent_client_protocol_core::{Conductor, ConnectTo, Proxy};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Parameters for the echo tool
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct EchoParams {
    /// The message to echo back
    message: String,
}

/// Output from the echo tool
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct EchoOutput {
    /// The echoed message
    result: String,
}

pub struct ProxyComponent;

impl ConnectTo<Conductor> for ProxyComponent {
    async fn connect_to(self, client: impl ConnectTo<Proxy>) -> Result<(), agent_client_protocol_core::Error> {
        let test_server = McpServer::builder("test")
            .instructions("A simple test MCP server with an echo tool")
            .tool_fn_mut(
                "echo",
                "Echoes back the input message",
                async |params: EchoParams, _context| {
                    Ok(EchoOutput {
                        result: format!("Echo: {}", params.message),
                    })
                },
                agent_client_protocol_core::tool_fn_mut!(),
            )
            .build();

        agent_client_protocol_core::Proxy
            .builder()
            .name("proxy-component")
            .with_mcp_server(test_server)
            .connect_to(client)
            .await
    }
}
