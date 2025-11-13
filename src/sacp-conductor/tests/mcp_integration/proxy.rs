//! Proxy component that provides MCP tools

use sacp::{Component, JrHandlerChain};
use sacp_proxy::{AcpProxyExt, McpServiceRegistry};

use crate::mcp_integration::mcp_server::TestMcpServer;

pub struct ProxyComponent;

impl Component for ProxyComponent {
    async fn serve(self, client: impl Component) -> Result<(), sacp::Error> {
        JrHandlerChain::new()
            .name("proxy-component")
            .provide_mcp(
                McpServiceRegistry::default().with_rmcp_server("test", TestMcpServer::new)?,
            )
            .proxy()
            .serve(client)
            .await
    }
}

pub fn create() -> sacp::DynComponent {
    sacp::DynComponent::new(ProxyComponent)
}
