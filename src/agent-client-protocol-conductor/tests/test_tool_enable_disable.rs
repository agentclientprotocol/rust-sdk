//! Integration tests for tool enable/disable functionality
//!
//! These tests verify that `disable_tool`, `enable_tool`, `disable_all_tools`,
//! and `enable_all_tools` correctly filter which tools are visible and callable.

use agent_client_protocol_core::mcp_server::McpServer;
use agent_client_protocol_core::{Conductor, ConnectTo, DynConnectTo, Proxy, RunWithConnectionTo};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use agent_client_protocol_test::testy::{Testy, TestyCommand};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Input for the echo tool
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct EchoInput {
    message: String,
}

/// Input for the greet tool
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct GreetInput {
    name: String,
}

/// Empty input for simple tools
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct EmptyInput {}

/// Create a proxy with multiple tools, some disabled via deny-list
fn create_proxy_with_disabled_tool() -> Result<DynConnectTo<Conductor>, agent_client_protocol_core::Error> {
    let mcp_server = McpServer::builder("test_server".to_string())
        .instructions("Test MCP server with some disabled tools")
        .tool_fn(
            "echo",
            "Echo a message back",
            async |input: EchoInput, _context| Ok(format!("Echo: {}", input.message)),
            agent_client_protocol_core::tool_fn!(),
        )
        .tool_fn(
            "greet",
            "Greet someone by name",
            async |input: GreetInput, _context| Ok(format!("Hello, {}!", input.name)),
            agent_client_protocol_core::tool_fn!(),
        )
        .tool_fn(
            "secret",
            "A secret tool that should be disabled",
            async |_input: EmptyInput, _context| Ok("This is secret!".to_string()),
            agent_client_protocol_core::tool_fn!(),
        )
        .disable_tool("secret")?
        .build();

    Ok(DynConnectTo::new(TestProxy { mcp_server }))
}

/// Create a proxy where all tools are disabled except specific ones (allow-list)
fn create_proxy_with_allowlist() -> Result<DynConnectTo<Conductor>, agent_client_protocol_core::Error> {
    let mcp_server = McpServer::builder("allowlist_server".to_string())
        .instructions("Test MCP server with allow-list")
        .tool_fn(
            "echo",
            "Echo a message back",
            async |input: EchoInput, _context| Ok(format!("Echo: {}", input.message)),
            agent_client_protocol_core::tool_fn!(),
        )
        .tool_fn(
            "greet",
            "Greet someone by name",
            async |input: GreetInput, _context| Ok(format!("Hello, {}!", input.name)),
            agent_client_protocol_core::tool_fn!(),
        )
        .tool_fn(
            "secret",
            "A secret tool",
            async |_input: EmptyInput, _context| Ok("This is secret!".to_string()),
            agent_client_protocol_core::tool_fn!(),
        )
        .disable_all_tools()
        .enable_tool("echo")?
        .build();

    Ok(DynConnectTo::new(TestProxy { mcp_server }))
}

struct TestProxy<R: RunWithConnectionTo<Conductor>> {
    mcp_server: McpServer<Conductor, R>,
}

impl<R: RunWithConnectionTo<Conductor> + 'static + Send> ConnectTo<Conductor> for TestProxy<R> {
    async fn connect_to(self, client: impl ConnectTo<Proxy>) -> Result<(), agent_client_protocol_core::Error> {
        agent_client_protocol_core::Proxy
            .builder()
            .name("test-proxy")
            .with_mcp_server(self.mcp_server)
            .connect_to(client)
            .await
    }
}

// ============================================================================
// Tests for deny-list (disable specific tools)
// ============================================================================

#[tokio::test]
async fn test_list_tools_excludes_disabled() -> Result<(), agent_client_protocol_core::Error> {
    let result = agent_client_protocol_yopo::prompt(
        ConductorImpl::new_agent(
            "test-conductor".to_string(),
            ProxiesAndAgent::new(Testy::new()).proxy(create_proxy_with_disabled_tool()?),
            Default::default(),
        ),
        TestyCommand::ListTools {
            server: "test_server".to_string(),
        }
        .to_prompt(),
    )
    .await?;

    // Should contain echo and greet, but NOT secret
    assert!(result.contains("echo"), "Expected 'echo' tool in list");
    assert!(result.contains("greet"), "Expected 'greet' tool in list");
    assert!(
        !result.contains("secret"),
        "Disabled 'secret' tool should not appear in list"
    );

    Ok(())
}

#[tokio::test]
async fn test_enabled_tool_can_be_called() -> Result<(), agent_client_protocol_core::Error> {
    let result = agent_client_protocol_yopo::prompt(
        ConductorImpl::new_agent(
            "test-conductor".to_string(),
            ProxiesAndAgent::new(Testy::new()).proxy(create_proxy_with_disabled_tool()?),
            Default::default(),
        ),
        TestyCommand::CallTool {
            server: "test_server".to_string(),
            tool: "echo".to_string(),
            params: serde_json::json!({"message": "hello"}),
        }
        .to_prompt(),
    )
    .await?;

    assert!(
        result.contains("Echo: hello"),
        "Expected echo response, got: {}",
        result
    );

    Ok(())
}

#[tokio::test]
async fn test_disabled_tool_returns_not_found() -> Result<(), agent_client_protocol_core::Error> {
    let result = agent_client_protocol_yopo::prompt(
        ConductorImpl::new_agent(
            "test-conductor".to_string(),
            ProxiesAndAgent::new(Testy::new()).proxy(create_proxy_with_disabled_tool()?),
            Default::default(),
        ),
        TestyCommand::CallTool {
            server: "test_server".to_string(),
            tool: "secret".to_string(),
            params: serde_json::json!({}),
        }
        .to_prompt(),
    )
    .await?;

    // Should get an error about tool not found
    assert!(
        result.contains("not found") || result.contains("error"),
        "Expected error for disabled tool, got: {}",
        result
    );

    Ok(())
}

// ============================================================================
// Tests for allow-list (disable all, enable specific)
// ============================================================================

#[tokio::test]
async fn test_allowlist_only_shows_enabled_tools() -> Result<(), agent_client_protocol_core::Error> {
    let result = agent_client_protocol_yopo::prompt(
        ConductorImpl::new_agent(
            "test-conductor".to_string(),
            ProxiesAndAgent::new(Testy::new()).proxy(create_proxy_with_allowlist()?),
            Default::default(),
        ),
        TestyCommand::ListTools {
            server: "allowlist_server".to_string(),
        }
        .to_prompt(),
    )
    .await?;

    // Should only contain echo
    assert!(result.contains("echo"), "Expected 'echo' tool in list");
    assert!(
        !result.contains("greet"),
        "'greet' should not appear (not in allow-list)"
    );
    assert!(
        !result.contains("secret"),
        "'secret' should not appear (not in allow-list)"
    );

    Ok(())
}

#[tokio::test]
async fn test_allowlist_enabled_tool_works() -> Result<(), agent_client_protocol_core::Error> {
    let result = agent_client_protocol_yopo::prompt(
        ConductorImpl::new_agent(
            "test-conductor".to_string(),
            ProxiesAndAgent::new(Testy::new()).proxy(create_proxy_with_allowlist()?),
            Default::default(),
        ),
        TestyCommand::CallTool {
            server: "allowlist_server".to_string(),
            tool: "echo".to_string(),
            params: serde_json::json!({"message": "allowed"}),
        }
        .to_prompt(),
    )
    .await?;

    assert!(
        result.contains("Echo: allowed"),
        "Expected echo response, got: {}",
        result
    );

    Ok(())
}

#[tokio::test]
async fn test_allowlist_non_enabled_tool_returns_not_found() -> Result<(), agent_client_protocol_core::Error> {
    let result = agent_client_protocol_yopo::prompt(
        ConductorImpl::new_agent(
            "test-conductor".to_string(),
            ProxiesAndAgent::new(Testy::new()).proxy(create_proxy_with_allowlist()?),
            Default::default(),
        ),
        TestyCommand::CallTool {
            server: "allowlist_server".to_string(),
            tool: "greet".to_string(),
            params: serde_json::json!({"name": "World"}),
        }
        .to_prompt(),
    )
    .await?;

    // greet is registered but not enabled, should error
    assert!(
        result.contains("not found") || result.contains("error"),
        "Expected error for non-enabled tool, got: {}",
        result
    );

    Ok(())
}
