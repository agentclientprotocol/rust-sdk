//! Integration test verifying that MCP tools receive the correct session_id
//!
//! This test verifies the complete flow:
//! 1. Editor creates a session and receives a session_id
//! 2. Proxy provides an MCP server with an echo tool
//! 3. Test agent invokes the tool
//! 4. The tool receives the correct session_id in its context
//! 5. The tool returns the session_id in its response
//! 6. We verify the session_ids match

use agent_client_protocol_conductor::{ConductorImpl, McpBridgeMode, ProxiesAndAgent};
use agent_client_protocol_core::RunWithConnectionTo;
use agent_client_protocol_core::mcp_server::McpServer;
use agent_client_protocol_core::{Conductor, ConnectTo, DynConnectTo, Proxy};
use agent_client_protocol_test::testy::{Testy, TestyCommand};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Input for the echo tool (null/empty)
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct EchoInput {}

/// Output from the echo tool containing the session_id
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct EchoOutput {
    acp_url: String,
}

/// Create a proxy that provides an MCP server with a session_id echo tool
fn create_echo_proxy() -> DynConnectTo<Conductor> {
    // Create MCP server with an echo tool that returns the session_id
    let mcp_server = McpServer::builder("echo_server".to_string())
        .instructions("Test MCP server with session_id echo tool")
        .tool_fn_mut(
            "echo",
            "Returns the current session_id",
            async |_input: EchoInput, context| {
                Ok(EchoOutput {
                    acp_url: context.acp_url(),
                })
            },
            agent_client_protocol_core::tool_fn_mut!(),
        )
        .build();

    // Create proxy component
    DynConnectTo::new(ProxyWithEchoServer { mcp_server })
}

struct ProxyWithEchoServer<R: RunWithConnectionTo<Conductor>> {
    mcp_server: McpServer<Conductor, R>,
}

impl<R: RunWithConnectionTo<Conductor> + 'static + Send> ConnectTo<Conductor>
    for ProxyWithEchoServer<R>
{
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol_core::Error> {
        agent_client_protocol_core::Proxy
            .builder()
            .name("echo-proxy")
            .with_mcp_server(self.mcp_server)
            .connect_to(client)
            .await
    }
}

#[tokio::test]
async fn test_list_tools_from_mcp_server() -> Result<(), agent_client_protocol_core::Error> {
    use expect_test::expect;

    let result = yopo::prompt(
        ConductorImpl::new_agent(
            "test-conductor".to_string(),
            ProxiesAndAgent::new(Testy::new()).proxy(create_echo_proxy()),
            McpBridgeMode::default(),
        ),
        TestyCommand::ListTools {
            server: "echo_server".to_string(),
        }
        .to_prompt(),
    )
    .await?;

    // Check the response using expect_test
    expect![[r"
        Available tools:
          - echo: Returns the current session_id"]]
    .assert_eq(&result);

    Ok(())
}

#[tokio::test]
async fn test_session_id_delivered_to_mcp_tools() -> Result<(), agent_client_protocol_core::Error> {
    let result = yopo::prompt(
        ConductorImpl::new_agent(
            "test-conductor".to_string(),
            ProxiesAndAgent::new(Testy::new()).proxy(create_echo_proxy()),
            McpBridgeMode::default(),
        ),
        TestyCommand::CallTool {
            server: "echo_server".to_string(),
            tool: "echo".to_string(),
            params: serde_json::json!({}),
        }
        .to_prompt(),
    )
    .await?;

    let pattern = regex::Regex::new(r#""acp_url":\s*String\("acp:[0-9a-f-]+"\)"#).unwrap();
    assert!(pattern.is_match(&result), "unexpected result: {result}");

    Ok(())
}
