//! Direct, request-scoped rmcp transport for ACP (no byte-stream emulation).

use std::{
    future::Future,
    sync::{Arc, Mutex},
};

use acp::{
    Role,
    mcp_server::{MCP_BACKEND_FAILURE, McpOutcome, McpRequest, McpRequestContext},
};
use agent_client_protocol as acp;
use futures::{
    channel::oneshot,
    future::{BoxFuture, Either},
};
use rmcp::{
    RoleServer, Service,
    model::ClientJsonRpcMessage,
    service::{self, NotificationContext, RequestContext},
    transport::OneshotTransport,
};
use tokio_util::sync::CancellationToken;

/// Reuses the same application service but owns every handler future and its
/// cancellation on this one operation.
struct OperationService<S> {
    app: Arc<S>,
    cancel: CancellationToken,
    completions: Arc<Mutex<Vec<oneshot::Receiver<()>>>>,
}

impl<S: Service<RoleServer>> Service<RoleServer> for OperationService<S> {
    fn handle_request(
        &self,
        request: <RoleServer as service::ServiceRole>::PeerReq,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<<RoleServer as service::ServiceRole>::Resp, rmcp::ErrorData>>
    + Send
    + '_ {
        let (done_tx, done_rx) = oneshot::channel();
        self.completions
            .lock()
            .expect("MCP operation poisoned")
            .push(done_rx);
        let cancel = self.cancel.clone();
        async move {
            let result = tokio::select! {
                biased;
                () = cancel.cancelled() => Err(rmcp::ErrorData::internal_error("operation cancelled", None)),
                result = self.app.handle_request(request, context) => result,
            };
            let _sent = done_tx.send(());
            result
        }
    }

    fn handle_notification(
        &self,
        notification: <RoleServer as service::ServiceRole>::PeerNot,
        context: NotificationContext<RoleServer>,
    ) -> impl Future<Output = Result<(), rmcp::ErrorData>> + Send + '_ {
        let (done_tx, done_rx) = oneshot::channel();
        self.completions
            .lock()
            .expect("MCP operation poisoned")
            .push(done_rx);
        let cancel = self.cancel.clone();
        async move {
            let result = tokio::select! {
                biased;
                () = cancel.cancelled() => Ok(()),
                result = self.app.handle_notification(notification, context) => result,
            };
            let _sent = done_tx.send(());
            result
        }
    }

    fn get_info(&self) -> <RoleServer as service::ServiceRole>::Info {
        self.app.get_info()
    }

    fn supported_protocol_versions(
        &self,
    ) -> std::borrow::Cow<'static, [rmcp::model::ProtocolVersion]> {
        self.app.supported_protocol_versions()
    }
}

/// Execute one request against shared rmcp application state. Neither the
/// client transport nor the rmcp server's actor is allowed to escape this call.
pub(crate) fn execute<R, S>(
    app: Arc<S>,
    request: McpRequest,
    context: McpRequestContext<R>,
) -> BoxFuture<'static, Result<McpOutcome, acp::Error>>
where
    R: Role,
    S: Service<RoleServer>,
{
    Box::pin(async move {
        let id = context.request_id().0.to_string();
        let raw = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": request.method,
            "params": request.params,
        });
        let inbound: ClientJsonRpcMessage = match serde_json::from_value(raw) {
            Ok(request) => request,
            Err(error) => {
                return Ok(McpOutcome::Error(
                    acp::schema::v1::McpError::new(
                        if error.to_string().contains("unknown variant") {
                            -32601
                        } else {
                            -32602
                        },
                        "Invalid MCP request",
                    )
                    .data(serde_json::Value::String(error.to_string())),
                ));
            }
        };
        let (transport, mut output) = OneshotTransport::<RoleServer>::new(inbound);
        let cancel = CancellationToken::new();
        let completions = Arc::new(Mutex::new(Vec::new()));
        let handler = OperationService {
            app,
            cancel: cancel.clone(),
            completions: completions.clone(),
        };
        let mut running = service::serve_directly_with_ct(handler, transport, None, cancel.clone());
        let operation = async {
            while let Some(outbound) = output.recv().await {
                let value = serde_json::to_value(outbound).map_err(|error| {
                    acp::Error::new(
                        acp::mcp_server::MCP_BACKEND_FAILURE,
                        format!("cannot serialize MCP output: {error}"),
                    )
                })?;
                match value {
                    serde_json::Value::Object(mut object) if object.contains_key("method") => {
                        if object.contains_key("id") {
                            return Err(acp::Error::new(
                                MCP_BACKEND_FAILURE,
                                "reverse MCP requests are not supported",
                            ));
                        }
                        let method = object
                            .remove("method")
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .ok_or_else(|| {
                                acp::Error::new(
                                    MCP_BACKEND_FAILURE,
                                    "MCP backend notification has no method",
                                )
                            })?;
                        let params = match object.remove("params") {
                            None | Some(serde_json::Value::Null) => None,
                            Some(serde_json::Value::Object(params)) => Some(params),
                            _ => {
                                return Err(acp::Error::new(
                                    MCP_BACKEND_FAILURE,
                                    "MCP backend notification parameters must be an object",
                                ));
                            }
                        };
                        context.send_notification(method, params).await?;
                    }
                    serde_json::Value::Object(mut object) if object.contains_key("result") => {
                        if object.get("id").and_then(serde_json::Value::as_str) != Some(id.as_str())
                        {
                            return Err(acp::Error::new(
                                MCP_BACKEND_FAILURE,
                                "MCP response ID mismatch",
                            ));
                        }
                        return Ok(McpOutcome::Result(
                            object.remove("result").expect("checked result"),
                        ));
                    }
                    serde_json::Value::Object(mut object) if object.contains_key("error") => {
                        if object.get("id").and_then(serde_json::Value::as_str) != Some(id.as_str())
                        {
                            return Err(acp::Error::new(
                                MCP_BACKEND_FAILURE,
                                "MCP error ID mismatch",
                            ));
                        }
                        let error =
                            serde_json::from_value(object.remove("error").expect("checked error"))
                                .map_err(|error| {
                                    acp::Error::new(
                                        acp::mcp_server::MCP_BACKEND_FAILURE,
                                        format!("invalid MCP error from backend: {error}"),
                                    )
                                })?;
                        return Ok(McpOutcome::Error(error));
                    }
                    _ => {
                        return Err(acp::Error::new(
                            MCP_BACKEND_FAILURE,
                            "unexpected MCP output",
                        ));
                    }
                }
            }
            Err(acp::Error::new(
                acp::mcp_server::MCP_BACKEND_FAILURE,
                "MCP backend closed without a response",
            ))
        };
        let cancelled = async {
            let acp = context.cancellation().cancelled();
            let operation = context.operation_cancellation().cancelled();
            futures::pin_mut!(acp, operation);
            let _reason = futures::future::select(acp, operation).await;
        };
        let result = match futures::future::select(Box::pin(operation), Box::pin(cancelled)).await {
            Either::Left((result, _)) => result,
            Either::Right(((), _)) => Err(acp::Error::request_cancelled()),
        };
        cancel.cancel();
        let closed = running.close().await;
        let handlers = std::mem::take(&mut *completions.lock().expect("MCP operation poisoned"));
        for completion in handlers {
            let _finished = completion.await;
        }
        closed.map_err(|error| {
            acp::Error::new(
                acp::mcp_server::MCP_BACKEND_FAILURE,
                format!("MCP backend cleanup failed: {error}"),
            )
        })?;
        result
    })
}
