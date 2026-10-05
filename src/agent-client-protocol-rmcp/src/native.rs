//! Direct OneshotTransport adapter; no hidden initialization or byte bridge.

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
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    service::{self, NotificationContext, RequestContext},
    transport::OneshotTransport,
};
use std::{
    future::Future,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

/// Tracks the actual user futures, not just the rmcp actor. rmcp may detach
/// handler tasks internally; close() alone does not prove they have been dropped.
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
        let (done, completion) = oneshot::channel();
        self.completions
            .lock()
            .expect("MCP operation poisoned")
            .push(completion);
        async move {
            // The select drops the losing user future before signaling done.
            let result = tokio::select! {
                biased;
                () = self.cancel.cancelled() => Err(rmcp::ErrorData::internal_error("operation cancelled", None)),
                result = self.app.handle_request(request, context) => result,
            };
            let _finished = done.send(());
            result
        }
    }
    fn handle_notification(
        &self,
        notification: <RoleServer as service::ServiceRole>::PeerNot,
        context: NotificationContext<RoleServer>,
    ) -> impl Future<Output = Result<(), rmcp::ErrorData>> + Send + '_ {
        let (done, completion) = oneshot::channel();
        self.completions
            .lock()
            .expect("MCP operation poisoned")
            .push(completion);
        async move {
            let result = tokio::select! {
                biased;
                () = self.cancel.cancelled() => Ok(()),
                result = self.app.handle_notification(notification, context) => result,
            };
            let _finished = done.send(());
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

fn failure(message: impl std::fmt::Display) -> acp::Error {
    acp::Error::new(MCP_BACKEND_FAILURE, "MCP backend failure").data(message.to_string())
}

async fn forward<R: Role>(
    output: ServerJsonRpcMessage,
    id: &str,
    context: &McpRequestContext<R>,
) -> Result<Option<McpOutcome>, acp::Error> {
    // Use raw JSON at the binding boundary so result and error payloads remain
    // opaque rather than converting through ACP's generic Error.
    let value = serde_json::to_value(output).map_err(failure)?;
    let serde_json::Value::Object(mut object) = value else {
        return Err(failure("unexpected MCP output"));
    };
    if object.contains_key("method") {
        if object.contains_key("id") {
            return Err(failure("reverse MCP requests are not supported"));
        }
        let method = object
            .remove("method")
            .and_then(|v| v.as_str().map(str::to_owned))
            .ok_or_else(|| failure("notification has no method"))?;
        let params = match object.remove("params") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::Object(params)) => Some(params),
            _ => return Err(failure("notification parameters must be an object")),
        };
        context.send_notification(method, params).await?;
        return Ok(None);
    }
    if object.get("id").and_then(serde_json::Value::as_str) != Some(id) {
        return Err(failure("MCP response ID mismatch"));
    }
    if let Some(result) = object.remove("result") {
        return Ok(Some(McpOutcome::Result(result)));
    }
    if let Some(error) = object.remove("error") {
        return Ok(Some(McpOutcome::Error(
            serde_json::from_value(error).map_err(failure)?,
        )));
    }
    Err(failure("unexpected MCP output"))
}

pub(crate) fn execute<R: Role, S: Service<RoleServer>>(
    app: Arc<S>,
    request: McpRequest,
    context: McpRequestContext<R>,
) -> BoxFuture<'static, Result<McpOutcome, acp::Error>> {
    Box::pin(async move {
        let id = context.request_id().0.to_string();
        let inbound: ClientJsonRpcMessage = match serde_json::from_value(serde_json::json!({
            "jsonrpc":"2.0", "id":id, "method":request.method, "params":request.params,
        })) {
            Ok(inbound) => inbound,
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
            while let Some(message) = output.recv().await {
                if let Some(outcome) = forward(message, &id, &context).await? {
                    return Ok(outcome);
                }
            }
            Err(failure("MCP backend closed without a response"))
        };
        let cancelled = async {
            let peer = context.cancellation().cancelled();
            let operation = context.operation_cancellation().cancelled();
            futures::pin_mut!(peer, operation);
            let _reason = futures::future::select(peer, operation).await;
        };
        // Cancellation wins if both are ready: no output after revocation.
        let result = match futures::future::select(Box::pin(cancelled), Box::pin(operation)).await {
            Either::Left(_) => Err(acp::Error::request_cancelled()),
            Either::Right((result, _)) => result,
        };
        cancel.cancel();
        let closed = running.close().await;
        let handlers = std::mem::take(&mut *completions.lock().expect("MCP operation poisoned"));
        for completion in handlers {
            let _finished = completion.await;
        }
        closed.map_err(failure)?;
        result
    })
}
