//! Direct in-memory ACP agent/client with a native MCP server and an rmcp client.
//!
//! Run: `cargo run -p agent-client-protocol-rmcp --example native_mcp_over_acp --features native_mcp_example`
//! The provider below explicitly forwards native ACP messages to its local
//! rmcp server to keep this example self-contained. The *agent* only needs
//! `McpOverAcp` to consume the declaration; no conductor or HTTP bridge runs.

use std::sync::{Arc, Mutex};

use agent_client_protocol::{
    Agent, ByteStreams, Channel, Client, ConnectionTo, Dispatch, Error, JsonRpcResponse, Responder,
    UntypedMessage,
    mcp_client::McpOverAcp,
    mcp_server::McpServer,
    role,
    schema::v1::{
        ConnectMcpRequest, ConnectMcpResponse, DisconnectMcpRequest, DisconnectMcpResponse,
        McpConnectionId, McpServerAcp, McpServerAcpId, MessageMcpNotification, MessageMcpRequest,
        MessageMcpResponse,
    },
};
use agent_client_protocol_rmcp::McpServerExt;
use futures::{StreamExt, channel::mpsc};
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct EchoParams {
    message: String,
}

#[derive(Clone, Debug)]
struct EchoServer {
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl EchoServer {
    #[tool(description = "Return the supplied message")]
    async fn echo(
        &self,
        Parameters(params): Parameters<EchoParams>,
    ) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![ContentBlock::text(
            params.message,
        )]))
    }
}

#[allow(unknown_lints, clippy::unused_async_trait_impl)]
#[tool_handler]
impl ServerHandler for EchoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("native-example", "1"))
    }
}

fn provider(
    active: Arc<Mutex<Option<mpsc::UnboundedSender<Dispatch>>>>,
    mut incoming: mpsc::UnboundedReceiver<Dispatch>,
) -> impl agent_client_protocol::ConnectTo<Agent> {
    let active_requests = active.clone();
    let active_notifications = active.clone();
    Client
        .builder()
        .on_receive_request(
            async |request: ConnectMcpRequest, responder: Responder<ConnectMcpResponse>, _| {
                if request.server_id.0.as_ref() != "echo" {
                    return responder.respond_with_error(Error::invalid_params());
                }
                responder.respond(ConnectMcpResponse::new(McpConnectionId::new(
                    "echo-connection",
                )))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: MessageMcpRequest, responder: Responder<MessageMcpResponse>, _| {
                if request.connection_id.0.as_ref() != "echo-connection" {
                    return responder.respond_with_error(Error::invalid_params());
                }
                let responder = responder.wrap_params(|method, result| {
                    result.and_then(|value: Value| MessageMcpResponse::from_value(method, value))
                });
                let active = active_requests.lock().unwrap();
                let Some(sender) = active.as_ref() else {
                    return responder.respond_with_error(Error::internal_error());
                };
                sender
                    .unbounded_send(Dispatch::Request(
                        UntypedMessage {
                            method: request.method,
                            params: request.params.map_or(Value::Null, Value::Object),
                        },
                        responder,
                    ))
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: MessageMcpNotification, _| {
                let active = active_notifications.lock().unwrap();
                let Some(sender) = active.as_ref() else {
                    return Ok(());
                };
                sender
                    .unbounded_send(Dispatch::Notification(UntypedMessage {
                        method: notification.method,
                        params: notification.params.map_or(Value::Null, Value::Object),
                    }))
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: DisconnectMcpRequest,
                        responder: Responder<DisconnectMcpResponse>,
                        _| {
                assert_eq!(request.connection_id.0.as_ref(), "echo-connection");
                active.lock().unwrap().take();
                responder.respond(DisconnectMcpResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .with_spawned(move |_acp: ConnectionTo<Agent>| async move {
            let server: McpServer<role::mcp::Client> =
                McpServer::from_rmcp("echo", || EchoServer {
                    tool_router: EchoServer::tool_router(),
                });
            role::mcp::Client
                .builder()
                .connect_with(server, async |mcp| {
                    while let Some(message) = incoming.next().await {
                        mcp.send_proxied_message_to(role::mcp::Server, message)?;
                    }
                    Ok(())
                })
                .await
        })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let declaration = McpServerAcp::new("echo", McpServerAcpId::new("echo"));
    let (acp_agent, acp_client) = Channel::duplex();
    let (outgoing, incoming) = mpsc::unbounded();
    let active = Arc::new(Mutex::new(Some(outgoing)));

    let agent = Agent.builder().connect_with(acp_agent, async |cx| {
        let (transport, close) = McpOverAcp::connect_v1(&cx, declaration.server_id.clone()).await?;
        let (sdk_stream, rmcp_stream) = tokio::io::duplex(8192);
        let (sdk_read, sdk_write) = tokio::io::split(sdk_stream);
        let (rmcp_read, rmcp_write) = tokio::io::split(rmcp_stream);
        let sdk_mcp = agent_client_protocol::ConnectTo::<role::mcp::Client>::connect_to(
            transport,
            ByteStreams::new(sdk_write.compat_write(), sdk_read.compat()),
        );
        let rmcp_client = async move {
            let service = rmcp::serve_client((), (rmcp_read, rmcp_write))
                .await
                .map_err(Error::into_internal_error)?;
            let tools = service
                .peer()
                .list_all_tools()
                .await
                .map_err(Error::into_internal_error)?;
            assert!(tools.iter().any(|tool| tool.name == "echo"));
            println!(
                "MCP tools over direct ACP: {:?}",
                tools.iter().map(|tool| &tool.name).collect::<Vec<_>>()
            );
            close.close().await?;
            drop(service);
            Ok::<_, Error>(())
        };
        futures::try_join!(sdk_mcp, rmcp_client)?;
        Ok(())
    });
    futures::try_join!(
        agent,
        agent_client_protocol::ConnectTo::<Agent>::connect_to(
            provider(active, incoming),
            acp_client
        )
    )?;
    Ok(())
}
