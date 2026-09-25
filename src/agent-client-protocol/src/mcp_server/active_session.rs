//! Request-scoped native MCP transport. Each ACP request owns execution and cleanup.

use futures::{
    StreamExt,
    channel::oneshot,
    future::{self, Either},
};
use serde_json::{Map, Value};
use std::{
    collections::HashMap,
    io::Write,
    marker::PhantomData,
    sync::{Arc, Mutex, Weak},
};

use crate::{
    Agent, Channel, ConnectTo, ConnectionTo, Dispatch, HandleDispatchFrom, Handled,
    JsonRpcNotification, JsonRpcRequest, RawJsonRpcMessage, Responder, Role, TransportFrame,
    mcp_server::{
        MCP_BACKEND_FAILURE, MCP_RESOURCE_EXHAUSTED, McpConnectionContext, McpConnectionTo,
        McpOperationCancellation, McpOutcome, McpRequest, McpRequestContext, McpServerConnect,
        McpService,
    },
    role::HasPeer,
    schema::v1::{
        McpError, McpRequestId, McpServerAcpId, MessageMcpNotification, MessageMcpRequest,
        MessageMcpResponse, RequestId,
    },
    util::MatchDispatchFrom,
};

// These bound admitted work and individual payloads.
const MAX_ACTIVE_REQUESTS: usize = 64;
const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const MCP_VERSION: &str = "2026-07-28";
type ActiveRequests = Arc<Mutex<HashMap<McpRequestId, oneshot::Sender<()>>>>;

pub(super) struct V1McpProtocol;
#[cfg(feature = "unstable_protocol_v2")]
pub(super) struct V2McpProtocol;

pub(super) trait McpProtocol: Send + 'static {
    type MessageRequest: JsonRpcRequest<Response = MessageMcpResponse>;
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

fn into_mcp_error(error: crate::Error) -> McpError {
    let mut mcp = McpError::new(error.code.into(), error.message);
    if let Some(data) = error.data {
        mcp = mcp.data(data);
    }
    mcp
}

fn outcome_response(outcome: McpOutcome) -> Result<MessageMcpResponse, crate::Error> {
    let response = match outcome {
        McpOutcome::Result(value) => MessageMcpResponse::success(value),
        McpOutcome::Error(error) => MessageMcpResponse::error(error),
    };
    check_payload_size(&response, MAX_PAYLOAD_BYTES)?;
    Ok(response)
}

fn project_outcome(outcome: McpOutcome, is_discovery: bool) -> Result<McpOutcome, crate::Error> {
    match outcome {
        McpOutcome::Result(mut value) if is_discovery => {
            constrain_discovery_versions(&mut value)?;
            Ok(McpOutcome::Result(value))
        }
        other => Ok(other),
    }
}

fn send_outcome(
    responder: Responder<MessageMcpResponse>,
    result: Result<McpOutcome, crate::Error>,
    is_discovery: bool,
) -> Result<(), crate::Error> {
    match result {
        Ok(outcome) => {
            // Projection failures are MCP outcomes; binding and size failures
            // remain named outer ACP errors, regardless of backend type.
            let outcome = project_outcome(outcome, is_discovery)
                .unwrap_or_else(|error| McpOutcome::Error(into_mcp_error(error)));
            match outcome_response(outcome) {
                Ok(response) => responder.respond(response),
                Err(error) => responder.respond_with_error(error),
            }
        }
        Err(error) => responder.respond_with_error(error),
    }
}

#[cfg(feature = "unstable_protocol_v2")]
impl McpProtocol for V2McpProtocol {
    type MessageRequest = crate::schema::v2::MessageMcpRequest;
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
    service: Option<Arc<dyn McpService<Counterpart>>>,
    active: ActiveRequests,
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

fn admit_request(
    active: &ActiveRequests,
    id: McpRequestId,
) -> Result<(ActiveRequest, oneshot::Receiver<()>), crate::Error> {
    let (stop_tx, stop_rx) = oneshot::channel();
    let mut requests = active.lock().expect("MCP request registry poisoned");
    if requests.contains_key(&id) {
        return Err(crate::Error::invalid_params().data("duplicate active MCP requestId"));
    }
    if requests.len() >= MAX_ACTIVE_REQUESTS {
        return Err(
            crate::Error::new(MCP_RESOURCE_EXHAUSTED, "MCP active request limit exceeded")
                .data(serde_json::json!({"limit": MAX_ACTIVE_REQUESTS})),
        );
    }
    requests.insert(id.clone(), stop_tx);
    Ok((
        ActiveRequest {
            active: Arc::downgrade(active),
            id,
        },
        stop_rx,
    ))
}

/// Count serialized bytes without allocating another copy of a potentially large payload.
fn check_payload_size(value: &impl serde::Serialize, limit: usize) -> Result<(), crate::Error> {
    struct Budget(usize);
    impl Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("MCP payload limit exceeded"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(limit), value).map_err(|_| {
        crate::Error::new(MCP_RESOURCE_EXHAUSTED, "MCP payload limit exceeded")
            .data(serde_json::json!({"limitBytes": limit}))
    })
}

impl<Counterpart: Role, Protocol> McpActiveSession<Counterpart, Protocol>
where
    Counterpart: HasPeer<Agent>,
    Protocol: McpProtocol,
{
    pub fn new_with_service(
        server_id: McpServerAcpId,
        mcp_connect: Arc<dyn McpServerConnect<Counterpart>>,
        service: Option<Arc<dyn McpService<Counterpart>>>,
    ) -> Self {
        Self {
            server_id,
            mcp_connect,
            service,
            active: Arc::default(),
            protocol: PhantomData,
        }
    }

    fn handle_request(
        &mut self,
        request: Protocol::MessageRequest,
        responder: Responder<MessageMcpResponse>,
        connection: &ConnectionTo<Counterpart>,
    ) -> Result<Handled<(Protocol::MessageRequest, Responder<MessageMcpResponse>)>, crate::Error>
    {
        let server_id = Protocol::server_id(&request);
        if server_id != self.server_id {
            return Ok(Handled::No {
                message: (request, responder),
                retry: false,
            });
        }
        let request_id = Protocol::request_id(&request);
        let (method, params) = Protocol::into_request(request);
        if let Err(error) = check_payload_size(&(&method, &params, &request_id), MAX_PAYLOAD_BYTES)
        {
            responder.respond_with_error(error)?;
            return Ok(Handled::Yes);
        }
        if let Err(error) = validate_modern_request(&method, params.as_ref()) {
            responder.respond(outcome_response(McpOutcome::Error(into_mcp_error(error)))?)?;
            return Ok(Handled::Yes);
        }
        let (guard, stop_rx) = match admit_request(&self.active, request_id.clone()) {
            Ok(admitted) => admitted,
            Err(error) => {
                responder.respond_with_error(error)?;
                return Ok(Handled::Yes);
            }
        };

        if let Some(service) = self.service.clone() {
            let metadata = params
                .as_ref()
                .and_then(|params| params.get("_meta"))
                .and_then(Value::as_object)
                .expect("validated MCP metadata")
                .clone();
            let cancellation = responder.cancellation();
            let operation_cancellation = McpOperationCancellation::new();
            let alive = Arc::new(futures::lock::Mutex::new(true));
            let send_connection = connection.clone();
            let send_server_id = server_id.clone();
            let send_request_id = request_id.clone();
            let send_cancellation = cancellation.clone();
            let send_operation_cancellation = operation_cancellation.clone();
            let send_alive = alive.clone();
            let notify = Arc::new(move |method: String, params: Option<Map<String, Value>>| {
                let connection = send_connection.clone();
                let server_id = send_server_id.clone();
                let request_id = send_request_id.clone();
                let cancellation = send_cancellation.clone();
                let operation_cancellation = send_operation_cancellation.clone();
                let alive = send_alive.clone();
                let send = async move {
                    let active = alive.lock().await;
                    if !*active
                        || cancellation.is_cancelled()
                        || operation_cancellation.is_cancelled()
                    {
                        return Err(crate::Error::request_cancelled());
                    }
                    check_payload_size(&(&method, &params), MAX_PAYLOAD_BYTES)?;
                    let send = connection.send_notification_to_async(
                        Agent,
                        Protocol::notification(server_id, request_id, method, params),
                    );
                    futures::pin_mut!(send);
                    let cancelled = async {
                        let peer = cancellation.cancelled();
                        let operation = operation_cancellation.cancelled();
                        let shutdown = connection.shutdown_requested();
                        futures::pin_mut!(peer, operation, shutdown);
                        let peer_or_operation = future::select(peer, operation);
                        futures::pin_mut!(peer_or_operation);
                        let _reason = future::select(peer_or_operation, shutdown).await;
                    };
                    futures::pin_mut!(cancelled);
                    let result = match future::select(send, cancelled).await {
                        Either::Left((result, _)) => result,
                        Either::Right(((), _)) => Err(crate::Error::request_cancelled()),
                    };
                    drop(active);
                    result
                };
                Box::pin(send) as futures::future::BoxFuture<'static, Result<(), crate::Error>>
            });
            let context = McpRequestContext::new(
                server_id.clone(),
                request_id.clone(),
                McpConnectionTo {
                    context: McpConnectionContext::Acp {
                        server_id,
                        request_id,
                    },
                    connection: connection.clone(),
                    cleanup: Some(Arc::default()),
                },
                metadata,
                cancellation.clone(),
                operation_cancellation.clone(),
                notify,
            );
            let is_discovery = method == "server/discover";
            let shutdown_connection = connection.clone();
            connection.spawn(async move {
                let request = McpRequest { method, params };
                let cleanup_connection = context.connection().clone();
                let operation = service.execute(request, context);
                let stop = async {
                    let cancelled = cancellation.cancelled();
                    let shutdown = shutdown_connection.shutdown_requested();
                    futures::pin_mut!(cancelled);
                    futures::pin_mut!(shutdown);
                    let stop_rx = stop_rx;
                    futures::pin_mut!(stop_rx);
                    let cancel_or_shutdown = future::select(cancelled, shutdown);
                    futures::pin_mut!(cancel_or_shutdown);
                    let _reason = future::select(cancel_or_shutdown, stop_rx).await;
                };
                let result = match future::select(operation, Box::pin(stop)).await {
                    Either::Left((result, _)) => result,
                    Either::Right(((), operation)) => {
                        operation_cancellation.cancel();
                        *alive.lock().await = false;
                        // Do not discard the operation future: its completion
                        // includes rmcp handler cancellation and actor join.
                        drop(operation.await);
                        Err(crate::Error::request_cancelled())
                    }
                };
                *alive.lock().await = false;
                cleanup_connection.wait_cleanup().await;
                // Operation futures have been dropped and cannot send late output.
                drop(guard);
                let response = send_outcome(responder, result, is_discovery);
                if let Err(error) = response {
                    tracing::debug!(?error, "cannot send MCP response");
                }
                Ok(())
            })?;
            return Ok(Handled::Yes);
        }

        let cleanup_connection = McpConnectionTo {
            context: McpConnectionContext::Acp {
                server_id: server_id.clone(),
                request_id: request_id.clone(),
            },
            connection: connection.clone(),
            cleanup: Some(Arc::default()),
        };
        let backend = self.mcp_connect.connect(cleanup_connection.clone());
        let connection_for_task = connection.clone();
        let cancellation = responder.cancellation();
        let (mut client, server) = Channel::duplex();
        // Keep the operation admitted until its backend has actually stopped.
        let (backend_stop_tx, backend_stop_rx) = oneshot::channel::<()>();
        let (backend_done_tx, mut backend_done_rx) = oneshot::channel();
        let spawn_result = connection.spawn(async move {
            // Own (not merely borrow) the future so cancellation drops its
            // backend before the completion acknowledgement is published.
            let run = Box::pin(backend.connect_to(server));
            let outcome = match future::select(run, backend_stop_rx).await {
                Either::Left((result, _)) => result,
                Either::Right((_, _)) => Ok(()),
            };
            drop(backend_done_tx.send(outcome));
            Ok(())
        });
        if let Err(error) = spawn_result {
            drop(guard);
            responder.respond_with_error(error)?;
            return Ok(Handled::Yes);
        }
        let spawn_result = connection.spawn(async move {
            let inner_id = RequestId::Str(request_id.0.to_string());
            let is_discovery = method == "server/discover";
            let process = async {
                let raw = RawJsonRpcMessage::request(
                    method,
                    params.map_or(Value::Null, Value::Object),
                    inner_id.clone(),
                )?;
                client
                    .tx
                    .send_frame(TransportFrame::Single(raw))
                    .await
                    .map_err(crate::Error::into_internal_error)?;
                while let Some(budgeted) = client.rx.next().await {
                    let (frame, _permit) = budgeted.into_parts();
                    let TransportFrame::Single(message) = frame else {
                        return Err(crate::Error::new(
                            MCP_BACKEND_FAILURE,
                            "MCP backends must send individual valid JSON-RPC messages",
                        ));
                    };
                    if matches!(message, RawJsonRpcMessage::Response(_))
                        && message.response_id() != Some(&inner_id)
                    {
                        return Err(crate::Error::new(
                            MCP_BACKEND_FAILURE,
                            "MCP backend returned a different request ID",
                        ));
                    }
                    match message {
                        RawJsonRpcMessage::Response(response) => {
                            // Returning ends notification forwarding before the terminal reply.
                            return match response {
                                crate::schema::v1::Response::Result { result, .. } => {
                                    Ok(McpOutcome::Result(result))
                                }
                                crate::schema::v1::Response::Error { error, .. } => {
                                    Ok(McpOutcome::Error(into_mcp_error(error)))
                                }
                            };
                        }
                        RawJsonRpcMessage::Notification(notification) => {
                            check_payload_size(&notification, MAX_PAYLOAD_BYTES)?;
                            let params = match notification.params {
                                Some(params) => match params.into_value() {
                                    Value::Object(map) => Some(map),
                                    _ => {
                                        return Err(crate::Error::new(
                                            MCP_BACKEND_FAILURE,
                                            "MCP backend notification parameters must be an object",
                                        ));
                                    }
                                },
                                None => None,
                            };
                            connection_for_task
                                .send_notification_to_async(
                                    Agent,
                                    Protocol::notification(
                                        server_id.clone(),
                                        request_id.clone(),
                                        notification.method.to_string(),
                                        params,
                                    ),
                                )
                                .await?;
                        }
                        RawJsonRpcMessage::Request(_) => {
                            return Err(crate::Error::new(
                                MCP_BACKEND_FAILURE,
                                "reverse MCP requests are not supported",
                            ));
                        }
                    }
                }
                Err(crate::Error::new(
                    MCP_BACKEND_FAILURE,
                    "MCP backend closed without a response",
                ))
            };
            let result = cancellation
                .run_until_cancelled(async {
                    let process = process;
                    futures::pin_mut!(process);
                    let stop = async {
                        let _reason = future::select(
                            stop_rx,
                            Box::pin(connection_for_task.shutdown_requested()),
                        )
                        .await;
                    };
                    futures::pin_mut!(stop);
                    let work = async {
                        match future::select(process, stop).await {
                            Either::Left((result, _)) => result,
                            Either::Right(((), _)) => Err(crate::Error::request_cancelled()),
                        }
                    };
                    futures::pin_mut!(work);
                    match future::select(work, &mut backend_done_rx).await {
                        Either::Left((result, _)) => result,
                        Either::Right((Ok(Err(error)), _)) => Err(error),
                        // The backend can finish immediately after queueing its
                        // reply. Drain the channel before calling that an EOF.
                        Either::Right((Ok(Ok(())) | Err(_), work)) => work.await,
                    }
                })
                .await;
            // Revoking the channel stops any late output. A cancellation is only
            // caller-visible now; cleanup and ID release happen after backend exit.
            drop(backend_stop_tx);
            // The receiver can have already completed in the race above. Polling
            // it again then returns immediately; otherwise this joins cleanup.
            drop(backend_done_rx.await);
            cleanup_connection.wait_cleanup().await;
            drop(guard);
            let response = send_outcome(responder, result, is_discovery);
            if let Err(error) = response {
                tracing::debug!(?error, "cannot send request-scoped MCP response");
            }
            Ok(())
        });
        // A failed spawn drops its responder and backend stop sender with the task.
        spawn_result?;
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

/// Discovery describes the revisions available through this binding, not other
/// transports the hosted backend might also implement.
fn constrain_discovery_versions(result: &mut Value) -> Result<(), crate::Error> {
    let versions = result
        .get_mut("supportedVersions")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| crate::Error::internal_error().data("invalid MCP discovery result"))?;
    if !versions
        .iter()
        .any(|version| version.as_str() == Some(MCP_VERSION))
    {
        return Err(crate::Error::new(-32022, "Unsupported protocol version")
            .data(serde_json::json!({"requested": MCP_VERSION, "supported": versions})));
    }
    *versions = vec![Value::String(MCP_VERSION.to_owned())];
    Ok(())
}

fn validate_modern_request(
    method: &str,
    params: Option<&Map<String, Value>>,
) -> Result<(), crate::Error> {
    if method == "initialize" {
        return Err(
            crate::Error::method_not_found().data("native MCP requests do not use initialize")
        );
    }
    let meta = params
        .and_then(|params| params.get("_meta"))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            crate::Error::invalid_params().data("inner params._meta must be an object")
        })?;
    let version = meta
        .get("io.modelcontextprotocol/protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            crate::Error::invalid_params()
                .data("inner params._meta requires io.modelcontextprotocol/protocolVersion")
        })?;
    if version != MCP_VERSION {
        return Err(crate::Error::new(-32022, "Unsupported protocol version")
            .data(serde_json::json!({"requested": version, "supported": [MCP_VERSION]})));
    }
    if !meta
        .get("io.modelcontextprotocol/clientCapabilities")
        .is_some_and(Value::is_object)
    {
        return Err(crate::Error::invalid_params().data(
            "inner params._meta requires io.modelcontextprotocol/clientCapabilities object",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveRequests, MAX_ACTIVE_REQUESTS, MAX_PAYLOAD_BYTES, McpOutcome, admit_request,
        check_payload_size, constrain_discovery_versions, into_mcp_error, outcome_response,
        validate_modern_request,
    };
    use crate::{
        mcp_server::MCP_RESOURCE_EXHAUSTED,
        schema::v1::{McpError, McpRequestId},
    };
    use serde_json::json;

    #[test]
    fn both_outcome_branches_obey_the_binding_payload_limit() {
        for outcome in [
            McpOutcome::Result(json!("x".repeat(MAX_PAYLOAD_BYTES))),
            McpOutcome::Error(
                McpError::new(-32000, "peer error").data(json!("x".repeat(MAX_PAYLOAD_BYTES))),
            ),
        ] {
            let error = outcome_response(outcome).expect_err("oversized carrier must be rejected");
            assert_eq!(i32::from(error.code), MCP_RESOURCE_EXHAUSTED);
        }
        let result = outcome_response(McpOutcome::Result(serde_json::Value::Null)).unwrap();
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            json!({"result":null})
        );
    }

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

    #[test]
    fn unsupported_version_is_an_mcp_error_not_a_legacy_fallback() {
        let params = json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2025-11-25",
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        });
        let error = validate_modern_request("tools/call", params.as_object()).unwrap_err();
        assert_eq!(
            serde_json::to_value(error).unwrap(),
            json!({
                "code": -32022,
                "message": "Unsupported protocol version",
                "data": {"requested": "2025-11-25", "supported": ["2026-07-28"]}
            })
        );
    }

    #[test]
    fn native_request_admission_is_bounded_and_recovers_after_cleanup() {
        let active = ActiveRequests::default();
        let mut admitted = Vec::new();
        for index in 0..MAX_ACTIVE_REQUESTS {
            admitted
                .push(admit_request(&active, McpRequestId::new(format!("req-{index}"))).unwrap());
        }
        assert_eq!(active.lock().unwrap().len(), MAX_ACTIVE_REQUESTS);
        let duplicate = admit_request(&active, McpRequestId::new("req-0"))
            .err()
            .unwrap();
        assert_eq!(duplicate.code, crate::ErrorCode::InvalidParams);
        let overload = admit_request(&active, McpRequestId::new("extra"))
            .err()
            .unwrap();
        assert_eq!(i32::from(overload.code), MCP_RESOURCE_EXHAUSTED);
        drop(admitted.pop());
        let replacement = admit_request(&active, McpRequestId::new("replacement")).unwrap();
        assert_eq!(active.lock().unwrap().len(), MAX_ACTIVE_REQUESTS);
        drop(replacement);
        drop(admitted);
        assert!(active.lock().unwrap().is_empty());
    }

    #[test]
    fn payload_limits_count_json_escaping_without_building_an_extra_buffer() {
        let payload = json!({"text": "\n\n"});
        let encoded = serde_json::to_vec(&payload).unwrap();
        assert!(check_payload_size(&payload, encoded.len()).is_ok());
        assert!(check_payload_size(&payload, encoded.len() - 1).is_err());
    }

    #[test]
    fn discovery_reports_the_binding_version_without_changing_other_payload() {
        let mut result = json!({
            "resultType": "complete",
            "supportedVersions": ["2025-11-25", "2026-07-28"],
            "capabilities": {"tools": {}},
            "_meta": {"vendor/opaque": ["preserved"]}
        });
        constrain_discovery_versions(&mut result).unwrap();
        assert_eq!(result["supportedVersions"], json!(["2026-07-28"]));
        assert_eq!(result["_meta"]["vendor/opaque"], json!(["preserved"]));
        assert_eq!(result["capabilities"], json!({"tools": {}}));
        let mut unsupported = json!({"supportedVersions": ["2025-11-25"]});
        assert!(constrain_discovery_versions(&mut unsupported).is_err());
        assert!(constrain_discovery_versions(&mut json!({})).is_err());
    }

    #[test]
    fn mcp_validation_errors_use_inner_carrier_and_preserve_null_data() {
        let unsupported = validate_modern_request(
            "tools/list",
            json!({"_meta": {
                "io.modelcontextprotocol/protocolVersion": "2025-03-26",
                "io.modelcontextprotocol/clientCapabilities": {}
            }})
            .as_object(),
        )
        .expect_err("unsupported inner version");
        let response = outcome_response(McpOutcome::Error(into_mcp_error(unsupported))).unwrap();
        assert_eq!(
            serde_json::to_value(response).unwrap()["error"]["code"],
            -32022
        );
        let response = outcome_response(McpOutcome::Error(
            McpError::new(-32000, "opaque MCP error").data(serde_json::Value::Null),
        ))
        .unwrap();
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({"error": {"code": -32000, "message": "opaque MCP error", "data": null}})
        );
    }
}
