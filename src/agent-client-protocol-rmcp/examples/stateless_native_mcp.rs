//! Run with `cargo run -p agent-client-protocol-rmcp --example stateless_native_mcp
//! --features unstable_mcp_over_acp,unstable_protocol_v2`.
//! No MCP initialize or separate MCP transport: the client attaches an rmcp service
//! to an ACP session and the agent invokes it through `mcp/message`.

use agent_client_protocol::{
    Agent, Client, Error, Responder, V2ConnectionTo,
    mcp_server::McpServer,
    schema::{ProtocolVersion, v2},
};
use agent_client_protocol_rmcp::McpServerExt;
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

struct Echo;

impl ServerHandler for Echo {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    fn call_tool(
        &self,
        params: CallToolRequestParams,
        _cx: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResponse, ErrorData>> + Send {
        std::future::ready(if params.name == "echo" {
            Ok(CallToolResult::structured(json!({"echoed": params.arguments})).into())
        } else {
            Err(ErrorData::invalid_params("unknown tool", None))
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let (done_tx, done_rx) = oneshot::channel();
    let done_tx = Arc::new(Mutex::new(Some(done_tx)));
    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _cx: V2ConnectionTo<Client>| {
                responder.respond(
                    v2::InitializeResponse::new(
                        request.protocol_version,
                        v2::Implementation::new("echo-agent", "1"),
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
                let server_id = match request.mcp_servers.as_slice() {
                    [v2::McpServer::Acp(server)] if server.name == "echo" => {
                        server.server_id.clone()
                    }
                    other => panic!("unexpected MCP declaration: {other:?}"),
                };
                let done_tx = done_tx.lock().unwrap().take().expect("one session");
                let call_cx = cx.clone();
                cx.spawn(async move {
                    let mut params = json!({"name": "echo", "arguments": {"message": "hello ACP"}});
                    params["_meta"] = json!({
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {},
                        "io.modelcontextprotocol/clientInfo": {"name": "echo-agent", "version": "1"}
                    });
                    let response = call_cx
                        .send_request(
                            v2::MessageMcpRequest::new(server_id, "echo-1", "tools/call")
                                .params(params.as_object().expect("object params").clone()),
                        )
                        .block_task()
                        .await;
                    drop(done_tx.send(response));
                    Ok(())
                })?;
                responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                    "echo-session",
                )))
            },
            agent_client_protocol::on_receive_request!(),
        );

    Client
        .v2()
        .connect_with(agent, async move |cx| {
            cx.send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                v2::Implementation::new("echo-client", "1"),
            ))
            .block_task()
            .await?;
            cx.build_session(std::env::current_dir().map_err(Error::into_internal_error)?)
                .with_mcp_server(McpServer::<Agent>::from_rmcp("echo", || Echo))?
                .start_session()
                .block_task()
                .await?;
            let response = done_rx.await.map_err(Error::into_internal_error)??;
            println!("{}", response.0.get());
            Ok(())
        })
        .await
}
