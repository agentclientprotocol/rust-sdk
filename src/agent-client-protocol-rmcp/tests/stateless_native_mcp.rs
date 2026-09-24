//! Native ACP attachment of an rmcp service (not standalone MCP transport).
#![cfg(all(feature = "unstable_protocol_v2", feature = "unstable_mcp_over_acp"))]

use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use agent_client_protocol::{
    Agent, Client, Error, Responder, V2ConnectionTo,
    mcp_server::McpServer,
    schema::{ProtocolVersion, v2},
};
use agent_client_protocol_rmcp::McpServerExt;
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, InputRequiredResult,
        ServerCapabilities, ServerConfig, SubscriptionFilter,
    },
    service::{RequestContext, SubscriptionContext},
};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

fn meta(marker: &str) -> Value {
    json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {"elicitation": {"form": {}}},
        "io.modelcontextprotocol/clientInfo": {"name": "native-acp", "version": "1"},
        "example/marker": marker})
}

async fn message(
    cx: &V2ConnectionTo<Client>,
    server: &v2::McpServerAcpId,
    id: &str,
    method: &str,
    mut params: Value,
    marker: &str,
) -> Result<Value, Error> {
    params["_meta"] = meta(marker);
    let response = cx
        .send_request(
            v2::MessageMcpRequest::new(server.clone(), id.to_owned(), method)
                .params(params.as_object().expect("object params").clone()),
        )
        .block_task()
        .await?;
    serde_json::from_str(response.0.get()).map_err(Error::into_internal_error)
}

struct DropSignal(Arc<Mutex<Option<oneshot::Sender<()>>>>);
impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(tx) = self.0.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }
}

struct Service {
    _drop: DropSignal,
    started: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    stopped: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}
impl ServerHandler for Service {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
    }
    fn call_tool(
        &self,
        request: CallToolRequestParams,
        cx: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + Send {
        std::future::ready(match request.name.as_ref() {
            "retry" if request.request_state.is_none() => {
                let inputs = serde_json::from_value(json!({"confirmation": {
                    "method": "elicitation/create", "params": {"mode": "form",
                    "message": "Confirm", "requestedSchema": {"type": "object",
                    "properties": {"approved": {"type": "boolean"}}}}
                }}))
                .expect("valid elicitation");
                Ok(InputRequiredResult::new(Some(inputs), Some("retry-state".into())).into())
            }
            "retry" if request.request_state.as_deref() == Some("retry-state") => Ok(
                CallToolResult::structured(json!({"marker": cx.meta.get("example/marker"),
                    "responses": request.input_responses}))
                .into(),
            ),
            "echo" => Ok(CallToolResult::structured(
                json!({"marker": cx.meta.get("example/marker")}),
            )
            .into()),
            _ => Err(ErrorData::invalid_params(
                "unknown tool or state",
                Some(json!({"source": "rmcp"})),
            )),
        })
    }
    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        Some(requested.clone())
    }
    async fn listen(&self, cx: SubscriptionContext) -> Result<(), ErrorData> {
        let _stopped = DropSignal(self.stopped.clone());
        cx.sink()
            .notify_tool_list_changed()
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        if let Some(tx) = self.started.lock().unwrap().take() {
            let _ = tx.send(());
        }
        cx.cancelled().await;
        Ok(())
    }
}

async fn exercise(
    cx: V2ConnectionTo<Client>,
    server: v2::McpServerAcpId,
    started: oneshot::Receiver<()>,
    stopped: oneshot::Receiver<()>,
    dropped: oneshot::Receiver<()>,
) -> Result<v2::McpServerAcpId, Error> {
    let direct = message(
        &cx,
        &server,
        "direct-1",
        "tools/call",
        json!({"name": "echo", "arguments": {}}),
        "direct",
    )
    .await?;
    assert_eq!(direct["structuredContent"]["marker"], "direct");
    let discovered = message(
        &cx,
        &server,
        "discover-1",
        "server/discover",
        json!({}),
        "discover",
    )
    .await?;
    assert!(
        discovered["supportedVersions"]
            .as_array()
            .unwrap()
            .contains(&json!("2026-07-28"))
    );
    let first = message(
        &cx,
        &server,
        "retry-1",
        "tools/call",
        json!({"name": "retry", "arguments": {}}),
        "first",
    )
    .await?;
    assert_eq!(first["resultType"], "input_required");
    assert_eq!(
        first["inputRequests"]["confirmation"]["method"],
        "elicitation/create"
    );
    let responses = json!({"confirmation": {"action": "accept", "content": {"approved": true}}});
    let retry = message(
        &cx,
        &server,
        "retry-2",
        "tools/call",
        json!({"name": "retry", "arguments": {}, "requestState": first["requestState"],
            "inputResponses": responses}),
        "second",
    )
    .await?;
    assert_eq!(retry["structuredContent"]["marker"], "second");
    assert_eq!(retry["structuredContent"]["responses"], responses);
    let error = message(
        &cx,
        &server,
        "error-1",
        "tools/call",
        json!({"name": "missing", "arguments": {}}),
        "error",
    )
    .await
    .expect_err("rmcp error");
    assert_eq!(
        serde_json::to_value(error)?["data"],
        json!({"source": "rmcp"})
    );
    let mut params = json!({"notifications": {"toolsListChanged": true}});
    params["_meta"] = meta("listen");
    let subscription = cx.send_request(
        v2::MessageMcpRequest::new(server.clone(), "listen-1", "subscriptions/listen")
            .params(params.as_object().expect("object params").clone()),
    );
    started.await.map_err(Error::into_internal_error)?;
    let parallel = message(
        &cx,
        &server,
        "parallel-1",
        "tools/call",
        json!({"name": "echo", "arguments": {}}),
        "parallel",
    )
    .await?;
    assert_eq!(parallel["structuredContent"]["marker"], "parallel");
    subscription.cancel()?;
    stopped.await.map_err(Error::into_internal_error)?;
    dropped.await.map_err(Error::into_internal_error)?;
    Ok(server)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_acp_stateless_rmcp_lifecycle() -> Result<(), Error> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (start_tx, start_rx) = oneshot::channel();
        let (stop_tx, stop_rx) = oneshot::channel();
        let (drop_tx, drop_rx) = oneshot::channel();
        let (result_tx, result_rx) = oneshot::channel();
        let invocation = Arc::new(Mutex::new(Some((start_rx, stop_rx, drop_rx, result_tx))));
        let (notifications_tx, mut notifications_rx) = mpsc::unbounded_channel();
        let started = Arc::new(Mutex::new(Some(start_tx)));
        let stopped = Arc::new(Mutex::new(Some(stop_tx)));
        let dropped = Arc::new(Mutex::new(Some(drop_tx)));
        let agent = Agent
            .v2()
            .on_receive_request(
                async |request: v2::InitializeRequest,
                       responder: Responder<v2::InitializeResponse>,
                       _cx: V2ConnectionTo<Client>| {
                    responder.respond(
                        v2::InitializeResponse::new(
                            request.protocol_version,
                            v2::Implementation::new("native-rmcp-agent", "1"),
                        )
                        .capabilities(
                            v2::AgentCapabilities::new().session(
                                v2::SessionCapabilities::new().mcp(
                                    v2::McpCapabilities::new().acp(v2::McpAcpCapabilities::new()),
                                ),
                            ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: v2::NewSessionRequest,
                            responder: Responder<v2::NewSessionResponse>,
                            cx: V2ConnectionTo<Client>| {
                    let server = match request.mcp_servers.as_slice() {
                        [v2::McpServer::Acp(server)] if server.name == "real-rmcp" => {
                            server.server_id.clone()
                        }
                        other => panic!("unexpected declarations: {other:?}"),
                    };
                    let (start_rx, stop_rx, drop_rx, result_tx) =
                        invocation.lock().unwrap().take().expect("one session");
                    let call_cx = cx.clone();
                    cx.spawn(async move {
                        let result = exercise(call_cx, server, start_rx, stop_rx, drop_rx).await;
                        drop(result_tx.send(result));
                        Ok(())
                    })?;
                    responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                        "native-session",
                    )))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                async move |notification: v2::MessageMcpNotification,
                            _cx: V2ConnectionTo<Client>| {
                    notifications_tx
                        .send(notification)
                        .map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_notification!(),
            );

        Client.v2().connect_with(agent, async move |cx| {
            cx.send_request(v2::InitializeRequest::new(ProtocolVersion::V2,
                v2::Implementation::new("native-rmcp-client", "1"))).block_task().await?;
            let server = McpServer::<Agent>::from_rmcp("real-rmcp", move || Service {
                _drop: DropSignal(dropped.clone()),
                started: started.clone(), stopped: stopped.clone(),
            });
            cx.build_session(std::env::current_dir().map_err(Error::into_internal_error)?)
                .with_mcp_server(server)?.start_session().block_task().await?;
            let server_id = result_rx.await.map_err(Error::into_internal_error)??;
            let acknowledgment = notifications_rx.recv().await.expect("acknowledgment");
            let update = notifications_rx.recv().await.expect("filtered update");
            assert_eq!(acknowledgment.method, "notifications/subscriptions/acknowledged");
            assert_eq!(
                acknowledgment.params.as_ref().unwrap()["notifications"]["toolsListChanged"],
                json!(true),
                "the rmcp subscription must accept the requested notification filter"
            );
            assert_eq!(update.method, "notifications/tools/list_changed");
            for notification in [acknowledgment, update] {
                assert_eq!(notification.server_id, server_id);
                assert_eq!(notification.request_id.0.as_ref(), "listen-1");
                assert_eq!(notification.params.as_ref().unwrap()["_meta"]
                    ["io.modelcontextprotocol/subscriptionId"], json!("listen-1"));
            }
            Ok(())
        }).await
    })
    .await
    .expect("native ACP/rmcp operation or cleanup timed out")
}
