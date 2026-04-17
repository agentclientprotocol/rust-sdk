//! Integration test for `tool_fn` - stateless concurrent tools
//!
//! This test verifies that `tool_fn` works correctly for stateless tools
//! that don't need mutable state.

use agent_client_protocol::mcp_server::McpServer;
use agent_client_protocol::{Conductor, ConnectTo, DynConnectTo, Proxy, RunWithConnectionTo};
use agent_client_protocol_conductor::{ConductorImpl, McpBridgeMode, ProxiesAndAgent};
use agent_client_protocol_test::testy::{Testy, TestyCommand};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Input for the greet tool
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct GreetInput {
    name: String,
}

/// Create a proxy that provides an MCP server with a stateless greet tool
fn create_greet_proxy() -> DynConnectTo<Conductor> {
    // Create MCP server with a stateless greet tool using tool_fn
    let mcp_server = McpServer::builder("greet_server".to_string())
        .instructions("Test MCP server with stateless greet tool")
        .tool_fn(
            "greet",
            "Greet someone by name",
            async |input: GreetInput, _context| Ok(format!("Hello, {}!", input.name)),
            agent_client_protocol::tool_fn!(),
        )
        .build();

    // Create proxy component
    DynConnectTo::new(ProxyWithGreetServer { mcp_server })
}

struct ProxyWithGreetServer<R: RunWithConnectionTo<Conductor>> {
    mcp_server: McpServer<Conductor, R>,
}

impl<R: RunWithConnectionTo<Conductor> + 'static + Send> ConnectTo<Conductor>
    for ProxyWithGreetServer<R>
{
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol::Error> {
        Proxy
            .builder()
            .name("greet-proxy")
            .with_mcp_server(self.mcp_server)
            .connect_to(client)
            .await
    }
}

#[tokio::test]
async fn test_tool_fn_greet() -> Result<(), agent_client_protocol::Error> {
    let result = yopo::prompt(
        ConductorImpl::new_agent(
            "test-conductor".to_string(),
            ProxiesAndAgent::new(Testy::new()).proxy(create_greet_proxy()),
            McpBridgeMode::default(),
        ),
        TestyCommand::CallTool {
            server: "greet_server".to_string(),
            tool: "greet".to_string(),
            params: serde_json::json!({"name": "World"}),
        }
        .to_prompt(),
    )
    .await?;

    expect_test::expect![[r#"
        "OK: CallToolResult { content: [Annotated { raw: Text(RawTextContent { text: \"\\\"Hello, World!\\\"\", meta: None }), annotations: None }], structured_content: None, is_error: Some(false), meta: None }"
    "#]].assert_debug_eq(&result);

    Ok(())
}
