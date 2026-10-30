//! Direct native ACP call into a real rmcp service, without an MCP handshake.
//!
//! cargo run -p agent-client-protocol-rmcp --example stateless_native_mcp \
//!   --features rmcp_3,unstable_protocol_v2,unstable_mcp_over_acp

use agent_client_protocol::v2::{ConnectionTo, Responder, schema};
use agent_client_protocol::{Agent, Client, Error, mcp_server::McpServer, schema::ProtocolVersion};
use agent_client_protocol_rmcp::rmcp_3::McpServerExt;
extern crate rmcp_3 as rmcp;

use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
};
use serde_json::json;
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
    let mut done_tx = Some(done_tx);
    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: schema::InitializeRequest,
                   responder: Responder<schema::InitializeResponse>,
                   _cx: ConnectionTo<Client>| {
                responder.respond(
                    schema::InitializeResponse::new(
                        request.protocol_version,
                        schema::Implementation::new("echo-agent", "1"),
                    )
                    .capabilities(schema::AgentCapabilities::new().session(
                        schema::SessionCapabilities::new().mcp(
                            schema::McpCapabilities::new().acp(schema::McpAcpCapabilities::new()),
                        ),
                    )),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: schema::NewSessionRequest,
                        responder: Responder<schema::NewSessionResponse>,
                        cx: ConnectionTo<Client>| {
                let [schema::McpServer::Acp(server)] = request.mcp_servers.as_slice() else {
                    return Err(Error::invalid_params().data("expected one native server"));
                };
                let server_id = server.server_id.clone();
                let done_tx = done_tx.take().expect("one example session");
                let call_cx = cx.clone();
                cx.spawn(async move {
                    let params = json!({
                        "name":"echo","arguments":{"message":"hello ACP"},
                        "_meta":{
                            "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                            "io.modelcontextprotocol/clientCapabilities":{}
                        }
                    });
                    let response = call_cx
                        .send_request(
                            schema::MessageMcpRequest::new(server_id, "echo-1", "tools/call")
                                .params(params.as_object().unwrap().clone()),
                        )
                        .block_task()
                        .await;
                    drop(done_tx.send(response));
                    Ok(())
                })?;
                responder.respond(schema::NewSessionResponse::new(schema::SessionId::new(
                    "echo-session",
                )))
            },
            agent_client_protocol::on_receive_request!(),
        );

    Client
        .v2()
        .connect_with(agent, async move |cx| {
            cx.send_request(schema::InitializeRequest::new(
                ProtocolVersion::V2,
                schema::Implementation::new("echo-client", "1"),
            ))
            .block_task()
            .await?;
            cx.build_session(std::env::current_dir().map_err(Error::into_internal_error)?)
                .with_mcp_server(McpServer::<Agent>::from_rmcp("echo", || Echo))?
                .start_session()
                .block_task()
                .await?;
            let response = done_rx.await.map_err(Error::into_internal_error)??;
            match response {
                schema::MessageMcpResponse::Result { result, .. } => println!("{result}"),
                schema::MessageMcpResponse::Error { error, .. } => {
                    eprintln!("MCP error {}: {}", error.code, error.message);
                }
                _ => return Err(Error::internal_error().data("unknown MCP carrier")),
            }
            Ok(())
        })
        .await
}
