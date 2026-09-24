//! Request-scoped native MCP transport. An ACP request owns exactly one backend instance.

use futures::{
    StreamExt,
    channel::oneshot,
    future::{self, Either},
};
use serde_json::{Map, Value};
use std::{
    collections::HashMap,
    marker::PhantomData,
    sync::{Arc, Mutex, Weak},
};

use crate::{
    Agent, Channel, ConnectTo, ConnectionTo, Dispatch, HandleDispatchFrom, Handled,
    JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, RawJsonRpcMessage, Responder, Role,
    TransportFrame,
    mcp_server::{McpConnectionContext, McpConnectionTo, McpServerConnect},
    role::HasPeer,
    schema::v1::{
        McpRequestId, McpServerAcpId, MessageMcpNotification, MessageMcpRequest,
        MessageMcpResponse, RequestId,
    },
    util::MatchDispatchFrom,
};

pub(super) struct V1McpProtocol;
#[cfg(feature = "unstable_protocol_v2")]
pub(super) struct V2McpProtocol;

pub(super) trait McpProtocol: Send + 'static {
    type MessageRequest: JsonRpcRequest<Response = Self::MessageResponse>;
    type MessageResponse: JsonRpcResponse;
    type MessageNotification: JsonRpcNotification;

    fn server_id(request: &Self::MessageRequest) -> McpServerAcpId;
    fn request_id(request: &Self::MessageRequest) -> McpRequestId;
    fn into_request(request: Self::MessageRequest) -> (String, Option<Map<String, Value>>);
    fn notification(
        server_id: McpServerAcpId,
        request_id: McpRequestId,
        method: String,
        params: Option<Map<String, Value>>,
    ) -> Self::MessageNotification;
}

impl McpProtocol for V1McpProtocol {
    type MessageRequest = MessageMcpRequest;
    type MessageResponse = MessageMcpResponse;
    type MessageNotification = MessageMcpNotification;

    fn server_id(request: &Self::MessageRequest) -> McpServerAcpId {
        request.server_id.clone()
    }
    fn request_id(request: &Self::MessageRequest) -> McpRequestId {
        request.request_id.clone()
    }
    fn into_request(request: Self::MessageRequest) -> (String, Option<Map<String, Value>>) {
        (request.method, request.params)
    }
    fn notification(
        server_id: McpServerAcpId,
        request_id: McpRequestId,
        method: String,
        params: Option<Map<String, Value>>,
    ) -> Self::MessageNotification {
        MessageMcpNotification::new(server_id, request_id, method).params(params)
    }
}

#[cfg(feature = "unstable_protocol_v2")]
impl McpProtocol for V2McpProtocol {
    type MessageRequest = crate::schema::v2::MessageMcpRequest;
    type MessageResponse = crate::schema::v2::MessageMcpResponse;
    type MessageNotification = crate::schema::v2::MessageMcpNotification;

    fn server_id(request: &Self::MessageRequest) -> McpServerAcpId {
        McpServerAcpId::new(request.server_id.0.clone())
    }
    fn request_id(request: &Self::MessageRequest) -> McpRequestId {
        McpRequestId::new(request.request_id.0.clone())
    }
    fn into_request(request: Self::MessageRequest) -> (String, Option<Map<String, Value>>) {
        (request.method, request.params)
    }
    fn notification(
        server_id: McpServerAcpId,
        request_id: McpRequestId,
        method: String,
        params: Option<Map<String, Value>>,
    ) -> Self::MessageNotification {
        crate::schema::v2::MessageMcpNotification::new(server_id.0, request_id.0, method)
            .params(params)
    }
}

/// Active operations belong to the handler; dropping the declaration closes every operation.
pub(super) struct McpActiveSession<Counterpart: Role, Protocol = V1McpProtocol> {
    server_id: McpServerAcpId,
    mcp_connect: Arc<dyn McpServerConnect<Counterpart>>,
    active: Arc<Mutex<HashMap<McpRequestId, oneshot::Sender<()>>>>,
    protocol: PhantomData<fn() -> Protocol>,
}

struct ActiveRequest {
    active: Weak<Mutex<HashMap<McpRequestId, oneshot::Sender<()>>>>,
    id: McpRequestId,
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        if let Some(active) = self.active.upgrade() {
            active
                .lock()
                .expect("MCP request registry poisoned")
                .remove(&self.id);
        }
    }
}

impl<Counterpart: Role, Protocol> McpActiveSession<Counterpart, Protocol>
where
    Counterpart: HasPeer<Agent>,
    Protocol: McpProtocol,
{
    pub fn new(
        server_id: McpServerAcpId,
        mcp_connect: Arc<dyn McpServerConnect<Counterpart>>,
    ) -> Self {
        Self {
            server_id,
            mcp_connect,
            active: Arc::default(),
            protocol: PhantomData,
        }
    }

    fn handle_request(
        &mut self,
        request: Protocol::MessageRequest,
        responder: Responder<Protocol::MessageResponse>,
        connection: &ConnectionTo<Counterpart>,
    ) -> Result<
        Handled<(
            Protocol::MessageRequest,
            Responder<Protocol::MessageResponse>,
        )>,
        crate::Error,
    > {
        let server_id = Protocol::server_id(&request);
        if server_id != self.server_id {
            return Ok(Handled::No {
                message: (request, responder),
                retry: false,
            });
        }
        let request_id = Protocol::request_id(&request);
        let (method, params) = Protocol::into_request(request);
        if let Err(error) = validate_modern_request(&method, params.as_ref()) {
            responder.respond_with_error(error)?;
            return Ok(Handled::Yes);
        }
        let (stop_tx, stop_rx) = oneshot::channel();
        let duplicate = {
            let mut active = self.active.lock().expect("MCP request registry poisoned");
            if active.contains_key(&request_id) {
                true
            } else {
                active.insert(request_id.clone(), stop_tx);
                false
            }
        };
        if duplicate {
            responder.respond_with_error(
                crate::Error::invalid_params().data("duplicate active MCP requestId"),
            )?;
            return Ok(Handled::Yes);
        }

        let guard = ActiveRequest {
            active: Arc::downgrade(&self.active),
            id: request_id.clone(),
        };
        let backend = self.mcp_connect.connect(McpConnectionTo {
            context: McpConnectionContext::Acp {
                server_id: server_id.clone(),
                request_id: request_id.clone(),
            },
            connection: connection.clone(),
        });
        let connection_for_task = connection.clone();
        let cancellation = responder.cancellation();
        let (mut client, server) = Channel::duplex();
        // Dropping this sender when the request completes stops the backend even if it
        // has outstanding work after emitting its final response.
        let (backend_stop_tx, backend_stop_rx) = oneshot::channel::<()>();
        let spawn_result = connection.spawn(async move {
            let run = backend.connect_to(server);
            futures::pin_mut!(run);
            let stop = backend_stop_rx;
            futures::pin_mut!(stop);
            match future::select(run, stop).await {
                Either::Left((Err(error), _)) => {
                    tracing::warn!(?error, "request-scoped MCP backend failed");
                }
                Either::Left((Ok(()), _)) | Either::Right((_, _)) => {}
            }
            Ok(())
        });
        if let Err(error) = spawn_result {
            drop(guard);
            responder.respond_with_error(error)?;
            return Ok(Handled::Yes);
        }
        let spawn_result = connection.spawn(async move {
            let inner_id = RequestId::Str(request_id.0.to_string());
            let process = async {
                let raw = RawJsonRpcMessage::request(
                    method,
                    params.map_or(Value::Null, Value::Object),
                    inner_id.clone(),
                )?;
                client
                    .tx
                    .unbounded_send(TransportFrame::Single(raw))
                    .map_err(crate::Error::into_internal_error)?;
                while let Some(frame) = client.rx.next().await {
                    let mut result = None;
                    frame.inspect_messages(&mut |message| {
                        // A response ends the request, even within a batch. Notifications
                        // following it must not escape after the operation has completed.
                        if result.is_some() {
                            return Ok(());
                        }
                        match message {
                            RawJsonRpcMessage::Response(response) => {
                                if message.response_id() != Some(&inner_id) {
                                    return Err(crate::Error::invalid_params()
                                        .data("MCP backend returned a different request ID"));
                                }
                                result = Some(match response {
                                    crate::schema::v1::Response::Result { result, .. } => {
                                        Ok(result.clone())
                                    }
                                    crate::schema::v1::Response::Error { error, .. } => {
                                        Err(error.clone())
                                    }
                                });
                            }
                            RawJsonRpcMessage::Notification(notification) => {
                                let params = match notification.params.clone() {
                                    Some(params) => match params.into_value() {
                                        Value::Object(map) => Some(map),
                                        _ => return Err(crate::Error::invalid_params().data(
                                            "MCP backend notification parameters must be an object",
                                        )),
                                    },
                                    None => None,
                                };
                                connection_for_task.send_notification_to(
                                    Agent,
                                    Protocol::notification(
                                        server_id.clone(),
                                        request_id.clone(),
                                        notification.method.to_string(),
                                        params,
                                    ),
                                )?;
                            }
                            RawJsonRpcMessage::Request(_) => {
                                return Err(crate::Error::method_not_found()
                                    .data("reverse MCP requests are not supported"));
                            }
                        }
                        Ok(())
                    })?;
                    if let Some(response) = result {
                        return response;
                    }
                }
                Err(crate::util::internal_error(
                    "MCP backend closed without a response",
                ))
            };
            let result = cancellation
                .run_until_cancelled(async {
                    let process = process;
                    futures::pin_mut!(process);
                    let stop = stop_rx;
                    futures::pin_mut!(stop);
                    match future::select(process, stop).await {
                        Either::Left((result, _)) => result,
                        Either::Right((_, _)) => Err(crate::Error::request_cancelled()),
                    }
                })
                .await;
            // No more notifications can be forwarded after `process` is dropped.
            // Release the ID before publishing the final response so a caller can
            // immediately reuse it for the next independent operation.
            drop(backend_stop_tx);
            drop(guard);
            let response = match result {
                Ok(value) => match Protocol::MessageResponse::from_value("mcp/message", value) {
                    Ok(response) => responder.respond(response),
                    Err(error) => responder.respond_with_error(error),
                },
                Err(error) => responder.respond_with_error(error),
            };
            if let Err(error) = response {
                tracing::debug!(?error, "cannot send request-scoped MCP response");
            }
            Ok(())
        });
        if let Err(error) = spawn_result {
            // The dropped task also drops its responder and backend stop sender.
            return Err(error);
        }
        Ok(Handled::Yes)
    }
}

impl<Counterpart: Role, Protocol: McpProtocol> HandleDispatchFrom<Counterpart>
    for McpActiveSession<Counterpart, Protocol>
where
    Counterpart: HasPeer<Agent>,
{
    fn describe_chain(&self) -> impl std::fmt::Debug {
        "McpServerRequests"
    }

    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        connection: ConnectionTo<Counterpart>,
    ) -> Result<Handled<Dispatch>, crate::Error> {
        MatchDispatchFrom::new(message, &connection)
            .if_request_from(
                Agent,
                async |request: Protocol::MessageRequest, responder| {
                    self.handle_request(request, responder, &connection)
                },
            )
            .await
            .done()
    }
}

fn validate_modern_request(
    method: &str,
    params: Option<&Map<String, Value>>,
) -> Result<(), crate::Error> {
    if method == "initialize" {
        return Err(
            crate::Error::invalid_params().data("native MCP requests do not use initialize")
        );
    }
    let meta = params
        .and_then(|params| params.get("_meta"))
        .and_then(Value::as_object);
    if meta
        .and_then(|meta| meta.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(Value::as_str)
        != Some("2026-07-28")
        || !meta
            .and_then(|meta| meta.get("io.modelcontextprotocol/clientCapabilities"))
            .is_some_and(Value::is_object)
    {
        return Err(crate::Error::invalid_params().data("inner params._meta requires io.modelcontextprotocol/protocolVersion 2026-07-28 and io.modelcontextprotocol/clientCapabilities object"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_modern_request;
    use serde_json::json;

    #[test]
    fn only_modern_request_metadata_is_accepted() {
        let modern = json!({"_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientCapabilities": {}, "requestState": {"opaque": true}}});
        assert!(validate_modern_request("tools/list", modern.as_object()).is_ok());
        assert!(validate_modern_request("initialize", modern.as_object()).is_err());
        assert!(validate_modern_request("tools/list", json!({"_meta": {"io.modelcontextprotocol/protocolVersion": "2025-03-26", "io.modelcontextprotocol/clientCapabilities": {}}}).as_object()).is_err());
        assert!(
            validate_modern_request(
                "tools/list",
                json!({"_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}})
                    .as_object()
            )
            .is_err()
        );
    }
}
