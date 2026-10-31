//! Exercise the adapter with modern MCP requests, without an initialization handshake.
#![cfg(feature = "rmcp_3")]

use std::{future::Future, time::Duration};

use agent_client_protocol::{
    ConnectionTo, Error, UntypedMessage, mcp_server::McpServer, role::mcp,
};
use agent_client_protocol_rmcp::rmcp_3::McpServerExt;
extern crate rmcp_3 as rmcp;

use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, InputRequiredResult,
        ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(10);
const MODERN_VERSION: &str = "2026-07-28";

fn params(mut params: Value, marker: &str) -> Value {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": MODERN_VERSION,
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {
            "name": "modern-adapter-test",
            "version": "1"
        },
        "example/marker": marker
    });
    params
}

async fn request(
    connection: &ConnectionTo<mcp::Server>,
    method: &str,
    params: Value,
) -> Result<Value, Error> {
    connection
        .send_request(UntypedMessage::new(method, params)?)
        .block_task()
        .await
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct Echo {
    message: String,
}

#[tokio::test]
async fn built_server_handles_modern_calls_before_discovery() -> Result<(), Error> {
    tokio::time::timeout(TIMEOUT, async {
        let server = McpServer::<mcp::Client>::builder("modern-tools")
            .tool_fn(
                "echo",
                "Echo a message",
                async |input: Echo, _cx| Ok(input),
                agent_client_protocol_rmcp::rmcp_3::tool_fn!(),
            )
            .build();

        mcp::Client
            .builder()
            .connect_with(server, async |connection| {
                // Neither MCP initialize nor discovery is a prerequisite for a tool call.
                let result = request(
                    &connection,
                    "tools/call",
                    params(
                        json!({"name": "echo", "arguments": {"message": "hello"}}),
                        "call",
                    ),
                )
                .await?;
                assert_eq!(result["resultType"], "complete");
                assert_eq!(result["structuredContent"], json!({"message": "hello"}));

                let discovery = request(
                    &connection,
                    "server/discover",
                    params(json!({}), "discover"),
                )
                .await?;
                assert!(
                    discovery["supportedVersions"]
                        .as_array()
                        .expect("discovery should list supported protocol versions")
                        .contains(&json!(MODERN_VERSION))
                );
                assert!(discovery["capabilities"]["tools"].is_object());

                let tools = request(&connection, "tools/list", params(json!({}), "list")).await?;
                assert_eq!(tools["resultType"], "complete");
                assert_eq!(tools["ttlMs"], 0);
                assert_eq!(tools["cacheScope"], "private");
                assert_eq!(tools["tools"][0]["name"], "echo");

                let mut legacy = params(json!({}), "legacy");
                legacy["_meta"]["io.modelcontextprotocol/protocolVersion"] = json!("2025-11-25");
                let tools = request(&connection, "tools/list", legacy).await?;
                assert!(tools.get("ttlMs").is_none());
                assert!(tools.get("cacheScope").is_none());

                let mut unsupported = params(json!({}), "unsupported");
                unsupported["_meta"]["io.modelcontextprotocol/protocolVersion"] =
                    json!("1900-01-01");
                let error = request(&connection, "tools/list", unsupported)
                    .await
                    .expect_err("the server must reject an unsupported per-request version");
                assert_eq!(serde_json::to_value(error)?["code"], -32022);

                // A failed request must not change the context for the next request.
                let tools =
                    request(&connection, "tools/list", params(json!({}), "after-error")).await?;
                assert_eq!(tools["tools"][0]["name"], "echo");
                Ok(())
            })
            .await
    })
    .await
    .expect("modern adapter test timed out")
}

#[test]
fn enabling_or_disabling_an_unknown_tool_is_a_local_error() {
    let builder = McpServer::<mcp::Client>::builder("tools").tool_fn(
        "echo",
        "Echo a message",
        async |input: Echo, _cx| Ok(input),
        agent_client_protocol_rmcp::rmcp_3::tool_fn!(),
    );
    let Err(error) = builder.disable_tool("ech") else {
        panic!("a typo must be reported");
    };
    assert_eq!(error.name(), "ech");
    assert_eq!(error.to_string(), "unknown MCP tool `ech`");
}

#[tokio::test]
async fn built_server_reports_its_name_and_tool_errors_as_results() -> Result<(), Error> {
    tokio::time::timeout(TIMEOUT, async {
        let server = McpServer::<mcp::Client>::builder("named-tools")
            .version("1.2.3")
            .tool_fn(
                "echo",
                "Echo a message",
                async |input: Echo, _cx| Ok(input),
                agent_client_protocol_rmcp::rmcp_3::tool_fn!(),
            )
            .tool_fn(
                "fail",
                "Always fails",
                async |_input: Echo, _cx| -> Result<Echo, Error> {
                    Err(Error::internal_error().data("tool exploded"))
                },
                agent_client_protocol_rmcp::rmcp_3::tool_fn!(),
            )
            .build();

        mcp::Client
            .builder()
            .connect_with(server, async |connection| {
                let discovery = request(
                    &connection,
                    "server/discover",
                    params(json!({}), "discover"),
                )
                .await?;
                assert_eq!(
                    discovery["_meta"]["io.modelcontextprotocol/serverInfo"],
                    json!({"name": "named-tools", "version": "1.2.3"})
                );

                let failed = request(
                    &connection,
                    "tools/call",
                    params(
                        json!({"name": "fail", "arguments": {"message": "x"}}),
                        "fail",
                    ),
                )
                .await?;
                assert_eq!(failed["isError"], true);
                assert!(
                    failed["content"][0]["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("tool exploded"))
                );

                let bad_arguments = request(
                    &connection,
                    "tools/call",
                    params(
                        json!({"name": "echo", "arguments": {"wrong": 1}}),
                        "bad-arguments",
                    ),
                )
                .await?;
                assert_eq!(bad_arguments["isError"], true);
                assert!(
                    bad_arguments["content"][0]["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("message"))
                );

                let unknown = request(
                    &connection,
                    "tools/call",
                    params(json!({"name": "missing", "arguments": {}}), "unknown"),
                )
                .await
                .expect_err("an unknown tool is a protocol error");
                assert_eq!(serde_json::to_value(unknown)?["code"], -32602);
                Ok(())
            })
            .await
    })
    .await
    .expect("named server test timed out")
}

struct RetryServer;

impl ServerHandler for RetryServer {
    fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + Send {
        std::future::ready(match request.request_state.as_deref() {
            None => {
                let input_requests = serde_json::from_value(json!({
                    "confirmation": {
                        "method": "elicitation/create",
                        "params": {
                            "mode": "form",
                            "message": "Confirm this test call",
                            "requestedSchema": {
                                "type": "object",
                                "properties": {"approved": {"type": "boolean"}},
                                "required": ["approved"]
                            }
                        }
                    }
                }))
                .expect("the test elicitation request should deserialize");
                Ok(InputRequiredResult::new(
                    Some(input_requests),
                    Some("opaque-test-state".to_owned()),
                )
                .into())
            }
            Some("opaque-test-state") => Ok(CallToolResult::structured(json!({
                "marker": context.meta.get("example/marker"),
                "inputResponses": request.input_responses
            }))
            .into()),
            Some(_) => Err(ErrorData::invalid_params("unexpected retry state", None)),
        })
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
}

#[tokio::test]
async fn supplied_rmcp_service_preserves_mrtr_and_per_request_metadata() -> Result<(), Error> {
    tokio::time::timeout(TIMEOUT, async {
        let server = McpServer::<mcp::Client>::from_rmcp("retry-tools", || RetryServer);
        mcp::Client
            .builder()
            .connect_with(server, async |connection| {
                let mut first_params = params(json!({"name": "retry", "arguments": {}}), "first");
                first_params["_meta"]["io.modelcontextprotocol/clientCapabilities"] =
                    json!({"elicitation": {"form": {}}});
                let first = request(&connection, "tools/call", first_params).await?;
                assert_eq!(first["resultType"], "input_required");
                assert_eq!(first["requestState"], "opaque-test-state");
                assert_eq!(
                    first["inputRequests"]["confirmation"]["method"],
                    "elicitation/create"
                );

                // This is a fresh JSON-RPC request with its own metadata, not a reverse RPC.
                let input_responses = json!({
                    "confirmation": {"action": "accept", "content": {"approved": true}}
                });
                let mut retry_params = params(
                    json!({
                        "name": "retry",
                        "arguments": {},
                        "requestState": first["requestState"],
                        "inputResponses": input_responses
                    }),
                    "second",
                );
                retry_params["_meta"]["io.modelcontextprotocol/clientCapabilities"] =
                    json!({"elicitation": {"form": {}}});
                let second = request(&connection, "tools/call", retry_params).await?;
                assert_eq!(second["resultType"], "complete");
                assert_eq!(second["structuredContent"]["marker"], "second");
                assert_eq!(
                    second["structuredContent"]["inputResponses"],
                    input_responses
                );
                Ok(())
            })
            .await
    })
    .await
    .expect("MRTR adapter test timed out")
}
