//! Integration test for `tool_fn` - stateless concurrent tools
//!
//! This test verifies that `tool_fn` works correctly for stateless tools
//! that don't need mutable state.

use agent_client_protocol::mcp_server::McpServer;
use agent_client_protocol::{Conductor, ConnectTo, DynConnectTo, Proxy, RunWithConnectionTo};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use agent_client_protocol_polyfill::mcp_over_acp::McpOverAcpPolyfill;
use agent_client_protocol_rmcp::McpServerExt as _;
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
            ProxiesAndAgent::new(Testy::new())
                .proxy(create_greet_proxy())
                .proxy(McpOverAcpPolyfill::http()),
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
        "OK: CallToolResult { result_type: Some(ResultType(\"complete\")), content: [Text(TextContent { text: \"\\\"Hello, World!\\\"\", meta: None, annotations: None })], structured_content: None, is_error: Some(false), meta: None }"
    "#]].assert_debug_eq(&result);

    Ok(())
}

/// A cancelled call must not poison the mutable runner, and queued work whose
/// result receiver has gone away must never enter the user's closure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_tool_fn_mut_keeps_acp_alive() -> Result<(), agent_client_protocol::Error> {
    use agent_client_protocol::{
        Agent, Client, Error, Responder, V2ConnectionTo,
        schema::{ProtocolVersion, v2},
    };
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::sync::oneshot;

    #[derive(Debug, Deserialize, Serialize, JsonSchema)]
    struct Input {
        name: String,
    }
    let (started_tx, started_rx) = oneshot::channel();
    let started = Arc::new(Mutex::new(Some(started_tx)));
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let (result_tx, result_rx) = oneshot::channel();
    let invocation = Arc::new(Mutex::new(Some((started_rx, result_tx))));
    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _cx: V2ConnectionTo<Client>| {
                responder.respond(
                    v2::InitializeResponse::new(
                        request.protocol_version,
                        v2::Implementation::new("runner-agent", "1"),
                    )
                    .capabilities(
                        v2::AgentCapabilities::new()
                            .session(v2::SessionCapabilities::new().mcp(
                                v2::McpCapabilities::new().acp(v2::McpAcpCapabilities::new()),
                            )),
                    ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::NewSessionRequest,
                        responder: Responder<v2::NewSessionResponse>,
                        cx: V2ConnectionTo<Client>| {
                let [v2::McpServer::Acp(declaration)] = request.mcp_servers.as_slice() else {
                    panic!("expected one ACP MCP server")
                };
                let server = declaration.server_id.clone();
                let (started_rx, result_tx) =
                    invocation.lock().unwrap().take().expect("one session");
                let call_cx = cx.clone();
                cx.spawn(async move {
                    let result = async {
                        let make_request = |id: &str, name: &str| {
                            let params = serde_json::json!({
                                "name": "hold",
                                "arguments": {"name": name},
                                "_meta": {
                                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                                    "io.modelcontextprotocol/clientCapabilities": {}
                                }
                            });
                            v2::MessageMcpRequest::new(server.clone(), id.to_owned(), "tools/call")
                                .params(params.as_object().expect("object params").clone())
                        };
                        let running = call_cx.send_request(make_request("running", "running"));
                        started_rx.await.map_err(Error::into_internal_error)?;
                        let queued = call_cx.send_request(make_request("queued", "queued"));
                        tokio::task::yield_now().await;
                        queued.cancel()?;
                        running.cancel()?;
                        for request in [running, queued] {
                            let failure =
                                request.block_task().await.expect_err("cancelled request");
                            assert_eq!(i32::from(failure.code), -32800);
                        }
                        let healthy = call_cx
                            .send_request(make_request("after", "after"))
                            .block_task()
                            .await?;
                        let v2::MessageMcpResponse::Result { result, .. } = healthy else {
                            panic!("healthy tool call did not produce an MCP result")
                        };
                        assert_eq!(result["isError"], false, "healthy result: {result}");
                        Ok::<_, Error>(())
                    }
                    .await;
                    let _sent = result_tx.send(result);
                    Ok(())
                })?;
                responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                    "runner-session",
                )))
            },
            agent_client_protocol::on_receive_request!(),
        );
    tokio::time::timeout(
        Duration::from_secs(10),
        Client.v2().connect_with(agent, async move |cx| {
            cx.send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                v2::Implementation::new("runner-client", "1"),
            ))
            .block_task()
            .await?;
            let recorded = calls.clone();
            let started = started.clone();
            let server = McpServer::<Agent>::builder("runner")
                .tool_fn_mut(
                    "hold",
                    "Hold a mutable runner",
                    async move |input: Input, _cx| {
                        recorded.lock().unwrap().push(input.name.clone());
                        if input.name == "running" {
                            if let Some(tx) = started.lock().unwrap().take() {
                                let _sent = tx.send(());
                            }
                            std::future::pending::<()>().await;
                        }
                        Ok(serde_json::json!({"value": input.name}))
                    },
                    agent_client_protocol::tool_fn_mut!(),
                )
                .build();
            cx.build_session(std::env::current_dir().map_err(Error::into_internal_error)?)
                .with_mcp_server(server)?
                .start_session()
                .block_task()
                .await?;
            result_rx.await.map_err(Error::into_internal_error)??;
            assert_eq!(*calls.lock().unwrap(), ["running", "after"]);
            Ok::<_, Error>(())
        }),
    )
    .await
    .expect("cancelled MCP tool call timed out")
}
