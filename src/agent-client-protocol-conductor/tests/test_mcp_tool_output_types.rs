//! Test MCP tools with various output types (string, integer, object)
//!
//! MCP structured output requires JSON objects. This test verifies behavior
//! when tools return non-object types like bare strings or integers.

use agent_client_protocol_core::mcp_server::McpServer;
use agent_client_protocol_core::{Conductor, ConnectTo, DynConnectTo, Proxy, RunWithConnectionTo};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use agent_client_protocol_test::testy::{Testy, TestyCommand};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Empty input for test tools
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct EmptyInput {}

/// Create a proxy with tools that return different types
fn create_test_proxy() -> Result<DynConnectTo<Conductor>, agent_client_protocol_core::Error> {
    let mcp_server = McpServer::builder("test_server".to_string())
        .instructions("Test MCP server with various output types")
        .tool_fn_mut(
            "return_string",
            "Returns a bare string",
            async |_input: EmptyInput, _context| Ok("hello world".to_string()),
            agent_client_protocol_core::tool_fn_mut!(),
        )
        .tool_fn_mut(
            "return_integer",
            "Returns a bare integer",
            async |_input: EmptyInput, _context| Ok(42i32),
            agent_client_protocol_core::tool_fn_mut!(),
        )
        .build();

    Ok(DynConnectTo::new(ProxyWithTestServer { mcp_server }))
}

struct ProxyWithTestServer<R: RunWithConnectionTo<Conductor>> {
    mcp_server: McpServer<Conductor, R>,
}

impl<R: RunWithConnectionTo<Conductor> + 'static + Send> ConnectTo<Conductor>
    for ProxyWithTestServer<R>
{
    async fn connect_to(self, client: impl ConnectTo<Proxy>) -> Result<(), agent_client_protocol_core::Error> {
        agent_client_protocol_core::Proxy
            .builder()
            .name("test-proxy")
            .with_mcp_server(self.mcp_server)
            .connect_to(client)
            .await
    }
}

#[tokio::test]
async fn test_tool_returning_string() -> Result<(), agent_client_protocol_core::Error> {
    let result = agent_client_protocol_yopo::prompt(
        ConductorImpl::new_agent(
            "test-conductor".to_string(),
            ProxiesAndAgent::new(Testy::new()).proxy(create_test_proxy()?),
            Default::default(),
        ),
        TestyCommand::CallTool {
            server: "test_server".to_string(),
            tool: "return_string".to_string(),
            params: serde_json::json!({}),
        }
        .to_prompt(),
    )
    .await?;

    // The result should contain "hello world" somewhere
    assert!(
        result.contains("hello world"),
        "expected 'hello world' in result: {result}"
    );

    Ok(())
}

#[tokio::test]
async fn test_tool_returning_integer() -> Result<(), agent_client_protocol_core::Error> {
    let result = agent_client_protocol_yopo::prompt(
        ConductorImpl::new_agent(
            "test-conductor".to_string(),
            ProxiesAndAgent::new(Testy::new()).proxy(create_test_proxy()?),
            Default::default(),
        ),
        TestyCommand::CallTool {
            server: "test_server".to_string(),
            tool: "return_integer".to_string(),
            params: serde_json::json!({}),
        }
        .to_prompt(),
    )
    .await?;

    // The result should contain "42" somewhere
    assert!(result.contains("42"), "expected '42' in result: {result}");

    Ok(())
}
