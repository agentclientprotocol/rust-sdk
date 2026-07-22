//! Integration tests for the context delivered to ACP-attached MCP tools.
//!
//! This verifies that an attached tool receives both identifiers defined by the
//! native MCP-over-ACP lifecycle: the server ID advertised during session setup
//! and the connection ID created by `mcp/connect`.

use agent_client_protocol::RunWithConnectionTo;
use agent_client_protocol::mcp_server::McpServer;
use agent_client_protocol::{Conductor, ConnectTo, DynConnectTo, Proxy};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use agent_client_protocol_polyfill::mcp_over_acp::McpOverAcpPolyfill;
use agent_client_protocol_rmcp::McpServerExt as _;
use agent_client_protocol_test::testy::{Testy, TestyCommand};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct EchoInput {}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct EchoOutput {
    server_id: String,
    connection_id: String,
}

fn create_echo_proxy() -> DynConnectTo<Conductor> {
    let mcp_server = McpServer::builder("echo_server".to_string())
        .instructions("Test MCP server with a connection-context echo tool")
        .tool_fn_mut(
            "echo",
            "Returns the current MCP connection context",
            async |_input: EchoInput, context| {
                Ok(EchoOutput {
                    server_id: context
                        .server_id()
                        .expect("tool is attached through ACP")
                        .to_string(),
                    connection_id: context
                        .connection_id()
                        .expect("tool is attached through ACP")
                        .to_string(),
                })
            },
            agent_client_protocol::tool_fn_mut!(),
        )
        .build();

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
    ) -> Result<(), agent_client_protocol::Error> {
        agent_client_protocol::Proxy
            .builder()
            .name("echo-proxy")
            .with_mcp_server(self.mcp_server)
            .connect_to(client)
            .await
    }
}

#[tokio::test]
async fn test_list_tools_from_mcp_server() -> Result<(), agent_client_protocol::Error> {
    use expect_test::expect;

    let result = yopo::prompt(
        ConductorImpl::new_agent(
            "test-conductor".to_string(),
            ProxiesAndAgent::new(Testy::new())
                .proxy(create_echo_proxy())
                .proxy(McpOverAcpPolyfill::http()),
        ),
        TestyCommand::ListTools {
            server: "echo_server".to_string(),
        }
        .to_prompt(),
    )
    .await?;

    expect![[r"
        Available tools:
          - echo: Returns the current MCP connection context"]]
    .assert_eq(&result);

    Ok(())
}

#[tokio::test]
async fn test_acp_identifiers_are_delivered_to_mcp_tools()
-> Result<(), agent_client_protocol::Error> {
    let result = yopo::prompt(
        ConductorImpl::new_agent(
            "test-conductor".to_string(),
            ProxiesAndAgent::new(Testy::new())
                .proxy(create_echo_proxy())
                .proxy(McpOverAcpPolyfill::http()),
        ),
        TestyCommand::CallTool {
            server: "echo_server".to_string(),
            tool: "echo".to_string(),
            params: serde_json::json!({}),
        }
        .to_prompt(),
    )
    .await?;

    let server_id = regex::Regex::new(r#""server_id":\s*String\("mcp-server:[0-9a-f-]+"\)"#)
        .expect("valid server ID regex");
    let connection_id =
        regex::Regex::new(r#""connection_id":\s*String\("mcp-over-acp-connection:[0-9a-f-]+"\)"#)
            .expect("valid connection ID regex");
    assert!(server_id.is_match(&result), "unexpected result: {result}");
    assert!(
        connection_id.is_match(&result),
        "unexpected result: {result}"
    );

    Ok(())
}
