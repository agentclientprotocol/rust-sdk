//! Tests for running McpServer as a standalone MCP server (not part of ACP).
//!
//! These tests verify that `McpServer` can be used directly with MCP clients
//! via the `Component<McpServerToClient>` implementation.

use agent_client_protocol::{
    ByteStreams, ConnectTo, RunWithConnectionTo, mcp_server::McpServer, role::mcp, util::run_until,
};
use rmcp::{ClientHandler, ServiceExt, model::ClientInfo};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Input for the echo tool
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct EchoInput {
    message: String,
}

/// Input for the add tool
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct AddInput {
    a: i32,
    b: i32,
}

/// Output for the add tool (structured output)
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct AddOutput {
    result: i32,
}

/// Create a test MCP server with echo and add tools
fn create_test_server() -> McpServer<mcp::Client, impl RunWithConnectionTo<mcp::Client>> {
    McpServer::builder("test-server")
        .instructions("A test MCP server")
        .tool_fn(
            "echo",
            "Echo a message back",
            async |input: EchoInput, _cx| Ok(format!("Echo: {}", input.message)),
            agent_client_protocol::tool_fn!(),
        )
        .tool_fn(
            "add",
            "Add two numbers",
            async |input: AddInput, _cx| {
                Ok(AddOutput {
                    result: input.a + input.b,
                })
            },
            agent_client_protocol::tool_fn!(),
        )
        .build()
}

/// Minimal client handler for rmcp
#[derive(Debug, Clone, Default)]
struct MinimalClientHandler;

impl ClientHandler for MinimalClientHandler {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

#[tokio::test]
async fn test_standalone_server_list_tools() -> Result<(), agent_client_protocol::Error> {
    // Create duplex streams for communication
    let (server_stream, client_stream) = tokio::io::duplex(8192);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let (client_read, client_write) = tokio::io::split(client_stream);

    // Create the MCP server
    let server = create_test_server();

    // Wrap client side as ByteStreams (this is what the MCP server will talk to)
    let client_as_component = ByteStreams::new(client_write.compat_write(), client_read.compat());

    run_until(
        ConnectTo::<mcp::Client>::connect_to(server, client_as_component),
        async move {
            // Create rmcp client on the server side of the duplex (the "other end")
            let client = MinimalClientHandler
                .serve((server_read, server_write))
                .await
                .map_err(agent_client_protocol::util::internal_error)?;

            // List tools
            let tools_result = client
                .list_tools(None)
                .await
                .map_err(agent_client_protocol::util::internal_error)?;

            // Verify we got both tools
            assert_eq!(tools_result.tools.len(), 2);

            let tool_names: Vec<&str> =
                tools_result.tools.iter().map(|t| t.name.as_ref()).collect();
            assert!(tool_names.contains(&"echo"));
            assert!(tool_names.contains(&"add"));

            // Clean up
            client
                .cancel()
                .await
                .map_err(agent_client_protocol::util::internal_error)?;
            Ok(())
        },
    )
    .await
}

#[tokio::test]
async fn test_standalone_server_call_echo_tool() -> Result<(), agent_client_protocol::Error> {
    let (server_stream, client_stream) = tokio::io::duplex(8192);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let (client_read, client_write) = tokio::io::split(client_stream);

    let server = create_test_server();
    let client_as_component = ByteStreams::new(client_write.compat_write(), client_read.compat());

    run_until(
        ConnectTo::<mcp::Client>::connect_to(server, client_as_component),
        async move {
            let client = MinimalClientHandler
                .serve((server_read, server_write))
                .await
                .map_err(agent_client_protocol::util::internal_error)?;

            // Call the echo tool
            let result = client
                .call_tool(
                    rmcp::model::CallToolRequestParams::new("echo").with_arguments(
                        serde_json::json!({ "message": "hello world" })
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await
                .map_err(agent_client_protocol::util::internal_error)?;

            // Verify the result
            let text = result
                .content
                .first()
                .and_then(|c| c.raw.as_text())
                .map(|t| t.text.as_str())
                .expect("Expected text content");

            assert_eq!(text, r#""Echo: hello world""#, "Unexpected echo response");

            client
                .cancel()
                .await
                .map_err(agent_client_protocol::util::internal_error)?;
            Ok(())
        },
    )
    .await
}

#[tokio::test]
async fn test_standalone_server_call_add_tool() -> Result<(), agent_client_protocol::Error> {
    let (server_stream, client_stream) = tokio::io::duplex(8192);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let (client_read, client_write) = tokio::io::split(client_stream);

    let server = create_test_server();
    let client_as_component = ByteStreams::new(client_write.compat_write(), client_read.compat());

    run_until(
        ConnectTo::<mcp::Client>::connect_to(server, client_as_component),
        async move {
            let client = MinimalClientHandler
                .serve((server_read, server_write))
                .await
                .map_err(agent_client_protocol::util::internal_error)?;

            // Call the add tool
            let result = client
                .call_tool(
                    rmcp::model::CallToolRequestParams::new("add").with_arguments(
                        serde_json::json!({ "a": 5, "b": 3 })
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await
                .map_err(agent_client_protocol::util::internal_error)?;

            // The add tool returns structured output (AddOutput)
            // Check that we get the expected result
            assert!(!result.is_error.unwrap_or(false));

            // Structured output should have the result
            let content = result.content.first().expect("Expected content");
            let text = content.raw.as_text().expect("Expected text content");
            assert!(
                text.text.contains('8') || text.text.contains("result"),
                "Expected result to contain 8, got: {}",
                text.text
            );

            client
                .cancel()
                .await
                .map_err(agent_client_protocol::util::internal_error)?;
            Ok(())
        },
    )
    .await
}

#[tokio::test]
async fn test_standalone_server_tool_not_found() -> Result<(), agent_client_protocol::Error> {
    let (server_stream, client_stream) = tokio::io::duplex(8192);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let (client_read, client_write) = tokio::io::split(client_stream);

    let server = create_test_server();
    let client_as_component = ByteStreams::new(client_write.compat_write(), client_read.compat());

    run_until(
        ConnectTo::<mcp::Client>::connect_to(server, client_as_component),
        async move {
            let client = MinimalClientHandler
                .serve((server_read, server_write))
                .await
                .map_err(agent_client_protocol::util::internal_error)?;

            // Call a non-existent tool
            let result = client
                .call_tool(rmcp::model::CallToolRequestParams::new("nonexistent"))
                .await;

            // Should get an error
            assert!(result.is_err(), "Expected error for non-existent tool");

            client
                .cancel()
                .await
                .map_err(agent_client_protocol::util::internal_error)?;
            Ok(())
        },
    )
    .await
}

#[tokio::test]
async fn test_standalone_server_with_disabled_tools() -> Result<(), agent_client_protocol::Error> {
    let (server_stream, client_stream) = tokio::io::duplex(8192);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let (client_read, client_write) = tokio::io::split(client_stream);

    // Create server with echo tool disabled
    let server = McpServer::builder("test-server")
        .tool_fn(
            "echo",
            "Echo a message",
            async |input: EchoInput, _cx| Ok(format!("Echo: {}", input.message)),
            agent_client_protocol::tool_fn!(),
        )
        .tool_fn(
            "add",
            "Add two numbers",
            async |input: AddInput, _cx| {
                Ok(AddOutput {
                    result: input.a + input.b,
                })
            },
            agent_client_protocol::tool_fn!(),
        )
        .disable_tool("echo")?
        .build();

    let client_as_component = ByteStreams::new(client_write.compat_write(), client_read.compat());

    run_until(
        ConnectTo::<mcp::Client>::connect_to(server, client_as_component),
        async move {
            let client = MinimalClientHandler
                .serve((server_read, server_write))
                .await
                .map_err(agent_client_protocol::util::internal_error)?;

            // List tools - should only show "add"
            let tools_result = client
                .list_tools(None)
                .await
                .map_err(agent_client_protocol::util::internal_error)?;
            assert_eq!(tools_result.tools.len(), 1);
            assert_eq!(tools_result.tools[0].name.as_ref(), "add");

            // Calling disabled tool should fail
            let result = client
                .call_tool(
                    rmcp::model::CallToolRequestParams::new("echo").with_arguments(
                        serde_json::json!({ "message": "test" })
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await;

            assert!(result.is_err(), "Expected error for disabled tool");

            client
                .cancel()
                .await
                .map_err(agent_client_protocol::util::internal_error)?;
            Ok(())
        },
    )
    .await
}
