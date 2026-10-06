//! Request-scoped native MCP execution and cleanup.

use super::{
    MCP_BACKEND_FAILURE, MCP_SERVER_UNAVAILABLE, McpConnectionContext, McpConnectionTo,
    McpOperationCancellation, McpOutcome, McpRequest, McpRequestContext, McpServerConnect,
    McpService,
};
use crate::{
    Agent, ConnectTo, ConnectionTo, Dispatch, HandleDispatchFrom, Handled, JsonRpcNotification,
    JsonRpcRequest, JsonRpcResponse, RawJsonRpcError, RawJsonRpcMessage, RawJsonRpcParams,
    RawJsonRpcResponse, RequestCancellation, Responder, Role, TransportFrame,
    jsonrpc::DynamicHandlerCleanup,
    role::HasPeer,
    schema::v1::{
        McpError, McpRequestId, McpServerAcpId, MessageMcpNotification, MessageMcpRequest,
        MessageMcpResponse, RequestId,
    },
    util::MatchDispatchFrom,
};
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

const MCP_VERSION: &str = "2026-07-28";
#[derive(Debug, Default)]
struct RequestRegistry {
    closed: bool,
    requests: HashMap<McpRequestId, Option<oneshot::Sender<()>>>,
    waiters: Vec<oneshot::Sender<()>>,
}
type Requests = Mutex<RequestRegistry>;
type ActiveRequests = Arc<Requests>;

#[derive(Debug, Clone)]
pub(super) struct RegistrationCleanup(ActiveRequests);

impl DynamicHandlerCleanup for RegistrationCleanup {
    fn close(&self) {
        let mut registry = self.0.lock().expect("MCP request registry poisoned");
        registry.closed = true;
        // Keep IDs until owned cleanup completes, but revoke admission and
        // output authority atomically before waking any supervisor.
        for stop in registry.requests.values_mut() {
            stop.take();
        }
        registry.wake_if_finished();
    }

    fn wait(&self) -> futures::future::BoxFuture<'static, ()> {
        let active = self.0.clone();
        Box::pin(async move {
            let rx = {
                let mut registry = active.lock().expect("MCP request registry poisoned");
                if registry.closed && registry.requests.is_empty() {
                    return;
                }
                let (tx, rx) = oneshot::channel();
                registry.waiters.push(tx);
                rx
            };
            let _ = rx.await;
        })
    }
}

impl RequestRegistry {
    fn wake_if_finished(&mut self) {
        if self.closed && self.requests.is_empty() {
            for waiter in self.waiters.drain(..) {
                let _ = waiter.send(());
            }
        }
    }
}

pub(super) struct V1McpProtocol;
#[cfg(feature = "unstable_protocol_v2")]
pub(super) struct V2McpProtocol;

pub(super) trait McpProtocol: Send + 'static {
    type MessageRequest: JsonRpcRequest<Response = Self::MessageResponse>;
    type MessageResponse: JsonRpcResponse;
    type MessageNotification: JsonRpcNotification;
    fn response(outcome: McpOutcome) -> Self::MessageResponse;
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
    fn response(outcome: McpOutcome) -> Self::MessageResponse {
        match outcome {
            McpOutcome::Result(value) => MessageMcpResponse::success(value),
            McpOutcome::Error(error) => MessageMcpResponse::error(error),
        }
    }
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
    fn response(outcome: McpOutcome) -> Self::MessageResponse {
        match outcome {
            McpOutcome::Result(value) => Self::MessageResponse::success(value),
            McpOutcome::Error(error) => {
                let mut wire = crate::schema::v2::McpError::new(error.code, error.message);
                wire.data = error.data;
                wire.extra = error.extra;
                Self::MessageResponse::error(wire)
            }
        }
    }
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
        Self::MessageNotification::new(server_id.0, request_id.0, method).params(params)
    }
}

fn into_mcp_error(error: impl Into<RawJsonRpcError>) -> McpError {
    let error = error.into();
    let mut mcp = McpError::new(error.code, error.message);
    mcp.data = error.data;
    mcp.extra = error.extra;
    mcp
}
fn backend_failure(error: impl std::fmt::Display) -> crate::Error {
    crate::Error::new(MCP_BACKEND_FAILURE, "MCP backend failure").data(error.to_string())
}

fn output_revoked<R: Role>(
    cancellation: &RequestCancellation,
    connection: &ConnectionTo<R>,
    provider: &Weak<Requests>,
) -> Option<crate::Error> {
    with_output_authority(cancellation, connection, provider, || Ok(())).err()
}

fn with_output_authority<R: Role, T>(
    cancellation: &RequestCancellation,
    connection: &ConnectionTo<R>,
    provider: &Weak<Requests>,
    send: impl FnOnce() -> Result<T, crate::Error>,
) -> Result<T, crate::Error> {
    use futures::FutureExt;
    if cancellation.is_cancelled()
        || std::pin::pin!(connection.shutdown_requested())
            .now_or_never()
            .is_some()
    {
        return Err(crate::Error::request_cancelled());
    }
    let unavailable = || crate::Error::new(MCP_SERVER_UNAVAILABLE, "MCP provider removed");
    let active = provider.upgrade().ok_or_else(unavailable)?;
    let registry = active.lock().expect("MCP request registry poisoned");
    if registry.closed {
        return Err(unavailable());
    }
    // Queue output while holding the same gate that seals admission on close.
    // A retained context cannot send a late notification after guard removal.
    send()
}

/// Handler removal closes explicitly, even when a scope retains cleanup state.
pub(super) struct McpActiveSession<Counterpart: Role, Protocol = V1McpProtocol> {
    server_id: McpServerAcpId,
    mcp_connect: Arc<dyn McpServerConnect<Counterpart>>,
    service: Option<Arc<dyn McpService<Counterpart>>>,
    active: ActiveRequests,
    protocol: PhantomData<fn() -> Protocol>,
}
struct ActiveRequest {
    active: ActiveRequests,
    id: McpRequestId,
}
impl Drop for ActiveRequest {
    fn drop(&mut self) {
        let mut registry = self.active.lock().expect("MCP request registry poisoned");
        registry.requests.remove(&self.id);
        registry.wake_if_finished();
    }
}

impl<Counterpart: Role, Protocol> Drop for McpActiveSession<Counterpart, Protocol> {
    fn drop(&mut self) {
        RegistrationCleanup(self.active.clone()).close();
    }
}

fn admit_request(
    active: &ActiveRequests,
    id: McpRequestId,
) -> Result<(ActiveRequest, oneshot::Receiver<()>), crate::Error> {
    let (tx, rx) = oneshot::channel();
    let mut requests = active.lock().expect("MCP request registry poisoned");
    if requests.closed {
        return Err(crate::Error::new(
            MCP_SERVER_UNAVAILABLE,
            "MCP provider removed",
        ));
    }
    if requests.requests.contains_key(&id) {
        return Err(crate::Error::invalid_params().data("duplicate active MCP requestId"));
    }
    requests.requests.insert(id.clone(), Some(tx));
    Ok((
        ActiveRequest {
            active: active.clone(),
            id,
        },
        rx,
    ))
}
async fn stopped<R: Role>(
    cancellation: &RequestCancellation,
    connection: &ConnectionTo<R>,
    removed: oneshot::Receiver<()>,
) -> crate::Error {
    let peer = cancellation.cancelled();
    let shutdown = connection.shutdown_requested();
    futures::pin_mut!(peer, shutdown);
    let cancelled = future::select(peer, shutdown);
    futures::pin_mut!(cancelled);
    match future::select(cancelled, removed).await {
        Either::Left(_) => crate::Error::request_cancelled(),
        Either::Right(_) => crate::Error::new(MCP_SERVER_UNAVAILABLE, "MCP provider removed"),
    }
}

impl<Counterpart: Role, Protocol: McpProtocol> McpActiveSession<Counterpart, Protocol>
where
    Counterpart: HasPeer<Agent>,
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

    pub(super) fn cleanup(&self) -> Arc<dyn DynamicHandlerCleanup> {
        Arc::new(RegistrationCleanup(self.active.clone()))
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
        let is_discovery = method == "server/discover";
        if let Err(error) = validate_modern_request(&method, params.as_ref()) {
            responder.respond(Protocol::response(McpOutcome::Error(into_mcp_error(error))))?;
            return Ok(Handled::Yes);
        }
        let (guard, removed) = match admit_request(&self.active, request_id.clone()) {
            Ok(admitted) => admitted,
            Err(error) => {
                responder.respond_with_error(error)?;
                return Ok(Handled::Yes);
            }
        };
        let cleanup_connection = McpConnectionTo {
            context: McpConnectionContext::Acp {
                server_id: server_id.clone(),
                request_id: request_id.clone(),
            },
            connection: connection.clone(),
            cleanup: Some(Arc::default()),
        };
        let cancellation = responder.cancellation();
        let task_connection = connection.clone();
        let connector = self.mcp_connect.clone();
        let service = self.service.clone();
        let provider = Arc::downgrade(&self.active);
        // One protected task atomically owns construction, driving, forwarding
        // and cleanup. Rejected admission cannot start half an operation.
        connection.spawn_protected(async move {
            let result = if let Some(service) = service {
                let operation_cancellation = McpOperationCancellation::new();
                let alive = Arc::new(Mutex::new(true));
                let notify = {
                    let alive = alive.clone();
                    let connection = task_connection.clone();
                    let server_id = server_id.clone();
                    let request_id = request_id.clone();
                    let cancellation = cancellation.clone();
                    let operation_cancellation = operation_cancellation.clone();
                    let provider = provider.clone();
                    Arc::new(move |method: String, params: Option<Map<String, Value>>| {
                        let result = {
                            let active = alive.lock().expect("MCP output gate poisoned");
                            if !*active || operation_cancellation.is_cancelled() {
                                Err(crate::Error::request_cancelled())
                            } else {
                                with_output_authority(&cancellation, &connection, &provider, || {
                                    connection.send_notification_to(
                                        Agent,
                                        Protocol::notification(
                                            server_id.clone(),
                                            request_id.clone(),
                                            method,
                                            params,
                                        ),
                                    )
                                })
                            }
                        };
                        Box::pin(future::ready(result))
                            as futures::future::BoxFuture<'static, Result<(), crate::Error>>
                    })
                };
                let metadata = params
                    .as_ref()
                    .and_then(|p| p.get("_meta"))
                    .and_then(Value::as_object)
                    .expect("validated metadata")
                    .clone();
                let context = McpRequestContext::new(
                    server_id,
                    request_id,
                    cleanup_connection.clone(),
                    metadata,
                    cancellation.clone(),
                    operation_cancellation.clone(),
                    notify,
                );
                let operation = service.execute(McpRequest { method, params }, context);
                let stop = stopped(&cancellation, &task_connection, removed);
                futures::pin_mut!(stop);
                let result = match future::select(stop, operation).await {
                    Either::Left((reason, operation)) => {
                        operation_cancellation.cancel();
                        *alive.lock().expect("MCP output gate poisoned") = false;
                        // Do not abandon cleanup by dropping the service future.
                        drop(operation.await);
                        Err(reason)
                    }
                    Either::Right((result, _)) => result.map_err(backend_failure),
                };
                *alive.lock().expect("MCP output gate poisoned") = false;
                result
            } else {
                let backend = connector.connect(cleanup_connection.clone());
                let (mut client, mut driver) = backend.into_channel_and_future();
                let inner_id = RequestId::Str(request_id.0.to_string());
                let result = {
                    let process = async {
                        let raw = RawJsonRpcMessage::request(
                            method,
                            params.map_or(Value::Null, Value::Object),
                            inner_id.clone(),
                        )?;
                        client
                            .tx
                            .unbounded_send(TransportFrame::Single(raw))
                            .map_err(backend_failure)?;
                        let mut backend_error = None;
                        loop {
                            // A ready stream can process many frames in one
                            // poll. Check output authority for every frame, not
                            // just when the outer select regains control.
                            if let Some(error) =
                                output_revoked(&cancellation, &task_connection, &provider)
                            {
                                return Err(error);
                            }
                            let frame = match driver.as_mut() {
                                Some(run) => match future::select(client.rx.next(), run).await {
                                    Either::Left((message, _)) => message,
                                    Either::Right((result, receive)) => {
                                        drop(receive);
                                        driver.take();
                                        backend_error = result.err();
                                        // Drain already accepted output, but escaped
                                        // senders cannot extend output authority.
                                        client.rx.close();
                                        continue;
                                    }
                                },
                                None => client.rx.next().await,
                            };
                            let Some(TransportFrame::Single(message)) = frame else {
                                return Err(backend_failure(backend_error.take().map_or_else(
                                    || "backend closed without a valid response".to_owned(),
                                    |e| e.to_string(),
                                )));
                            };
                            if matches!(message, RawJsonRpcMessage::Response(_))
                                && message.response_id() != Some(&inner_id)
                            {
                                return Err(backend_failure("response request ID mismatch"));
                            }
                            match message {
                                RawJsonRpcMessage::Response(response) => {
                                    return match response {
                                        RawJsonRpcResponse::Result { result, .. } => {
                                            Ok(McpOutcome::Result(result))
                                        }
                                        RawJsonRpcResponse::Error { error, .. } => {
                                            Ok(McpOutcome::Error(into_mcp_error(*error)))
                                        }
                                    };
                                }
                                RawJsonRpcMessage::Notification(notification) => {
                                    let params =
                                        match notification.params.map(RawJsonRpcParams::into_value)
                                        {
                                            None | Some(Value::Null) => None,
                                            Some(Value::Object(params)) => Some(params),
                                            _ => {
                                                return Err(backend_failure(
                                                    "notification parameters must be an object",
                                                ));
                                            }
                                        };
                                    with_output_authority(
                                        &cancellation,
                                        &task_connection,
                                        &provider,
                                        || {
                                            task_connection.send_notification_to(
                                                Agent,
                                                Protocol::notification(
                                                    server_id.clone(),
                                                    request_id.clone(),
                                                    notification.method.to_string(),
                                                    params,
                                                ),
                                            )
                                        },
                                    )?;
                                }
                                RawJsonRpcMessage::Request(_) => {
                                    return Err(backend_failure(
                                        "reverse MCP requests are not supported",
                                    ));
                                }
                            }
                        }
                    };
                    let stop = stopped(&cancellation, &task_connection, removed);
                    futures::pin_mut!(process, stop);
                    match future::select(stop, process).await {
                        Either::Left((reason, _)) => Err(reason),
                        Either::Right((result, _)) => result,
                    }
                };
                client.rx.close();
                drop(client);
                // Preserve optional/cooperative driver semantics.
                let cleanup = if let Some(mut driver) = driver.take() {
                    if driver.request_finish() {
                        driver.await.map_err(backend_failure)
                    } else {
                        Ok(())
                    }
                } else {
                    Ok(())
                };
                cleanup.and(result)
            };
            cleanup_connection.wait_cleanup().await;
            // Apply the ACP binding's version projection once for either
            // execution path, without changing opaque results of other methods.
            let result = result.and_then(|outcome| {
                if is_discovery {
                    project_discovery_versions(outcome)
                } else {
                    Ok(outcome)
                }
            });
            // Hold the ID until actual handler/closure cleanup and terminal
            // handoff. EOF needs cleanup, not an attempted response.
            let shutdown = task_connection.shutdown_requested();
            futures::pin_mut!(shutdown);
            let response = if futures::FutureExt::now_or_never(shutdown).is_some() {
                Ok(())
            } else {
                responder.respond_with_result(result.map(Protocol::response))
            };
            drop(guard);
            response
        })?;
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

/// Discovery reports revisions usable through this binding, not every
/// revision that the backend supports on its other transports.
fn project_discovery_versions(outcome: McpOutcome) -> Result<McpOutcome, crate::Error> {
    let McpOutcome::Result(mut result) = outcome else {
        return Ok(outcome);
    };
    let versions = result
        .get_mut("supportedVersions")
        .and_then(Value::as_array_mut)
        .filter(|versions| versions.iter().all(Value::is_string))
        .ok_or_else(|| backend_failure("invalid MCP discovery supportedVersions"))?;
    if !versions
        .iter()
        .any(|version| version.as_str() == Some(MCP_VERSION))
    {
        return Ok(McpOutcome::Error(
            McpError::new(-32022, "Unsupported protocol version")
                .data(serde_json::json!({"requested": MCP_VERSION, "supported": versions})),
        ));
    }
    *versions = vec![Value::String(MCP_VERSION.to_owned())];
    Ok(McpOutcome::Result(result))
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
        .and_then(|p| p.get("_meta"))
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
            .data(serde_json::json!({"requested":version,"supported":[MCP_VERSION]})));
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
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn removing_provider_revokes_retained_context_and_joins_active_operation() {
        struct Standalone;
        impl McpServerConnect<Agent> for Standalone {
            fn name(&self) -> String {
                "removal".into()
            }
            fn connect(
                &self,
                _: McpConnectionTo<Agent>,
            ) -> crate::DynConnectTo<crate::role::mcp::Client> {
                panic!("native service must not construct a standalone connector")
            }
        }
        struct Service {
            context: Mutex<Option<tokio::sync::oneshot::Sender<McpRequestContext<Agent>>>>,
            cleaning: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
            release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        }
        impl McpService<Agent> for Service {
            fn execute(
                &self,
                _: McpRequest,
                context: McpRequestContext<Agent>,
            ) -> futures::future::BoxFuture<'static, Result<McpOutcome, crate::Error>> {
                let context_tx = self.context.lock().unwrap().take().unwrap();
                let cleaning = self.cleaning.lock().unwrap().take().unwrap();
                let release = self.release.lock().unwrap().take().unwrap();
                Box::pin(async move {
                    context_tx.send(context.clone()).unwrap();
                    context.operation_cancellation().cancelled().await;
                    assert!(
                        context
                            .send_notification("notifications/progress", None)
                            .await
                            .is_err()
                    );
                    let _sent = cleaning.send(());
                    release.await.unwrap();
                    Ok(McpOutcome::Result(Value::Null))
                })
            }
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let (transport, mut peer) = crate::Channel::duplex();
            let (context_tx, context_rx) = tokio::sync::oneshot::channel();
            let (cleaning_tx, cleaning_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let (remove_tx, remove_rx) = tokio::sync::oneshot::channel();
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
            let (installed_tx, installed_rx) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(crate::Client.builder().connect_with(transport, async move |cx| {
                let handler = McpActiveSession::<Agent>::new_with_service(
                    McpServerAcpId::new("provider"), Arc::new(Standalone),
                    Some(Arc::new(Service {
                        context: Mutex::new(Some(context_tx)),
                        cleaning: Mutex::new(Some(cleaning_tx)),
                        release: Mutex::new(Some(release_rx)),
                    })),
                );
                let guard = cx.add_dynamic_handler(handler)?;
                let _sent = installed_tx.send(());
                remove_rx.await.unwrap();
                drop(guard);
                stop_rx.await.unwrap();
                Ok(())
            }));
            installed_rx.await.unwrap();
            let request = json!({"jsonrpc":"2.0","id":"outer","method":"mcp/message","params":{
                "serverId":"provider","requestId":"logical","method":"tools/list","params":{"_meta":{
                    "io.modelcontextprotocol/protocolVersion":MCP_VERSION,
                    "io.modelcontextprotocol/clientCapabilities":{}
                }}
            }});
            peer.tx.unbounded_send(TransportFrame::Single(serde_json::from_value(request).unwrap())).unwrap();
            let context = context_rx.await.unwrap();
            let _sent = remove_tx.send(());
            cleaning_rx.await.unwrap();
            assert!(context.send_notification("notifications/progress", None).await.is_err());
            assert!(!task.is_finished());
            let _sent = release_tx.send(());
            let TransportFrame::Single(response) = peer.rx.next().await.unwrap() else { panic!("single frame") };
            assert_eq!(serde_json::to_value(response).unwrap()["error"]["code"], MCP_SERVER_UNAVAILABLE);
            let _sent = stop_tx.send(());
            task.await.unwrap().unwrap();
        }).await.expect("provider removal cleanup timed out");
    }

    #[test]
    fn logical_ids_are_held_until_cleanup_and_can_then_be_reused() {
        let active = ActiveRequests::default();
        let id = McpRequestId::new("logical");
        let (guard, _) = admit_request(&active, id.clone()).unwrap();
        assert!(admit_request(&active, id.clone()).is_err());
        drop(guard);
        assert!(admit_request(&active, id).is_ok());
    }

    #[tokio::test]
    async fn retained_cleanup_closes_admission_and_waits_for_all_active_ids() {
        use futures::FutureExt;
        let active = ActiveRequests::default();
        let cleanup = RegistrationCleanup(active.clone());
        let (first, removed) = admit_request(&active, McpRequestId::new("first")).unwrap();
        let (second, _) = admit_request(&active, McpRequestId::new("second")).unwrap();
        cleanup.close();
        removed.await.unwrap_err();
        assert!(admit_request(&active, McpRequestId::new("late")).is_err());
        let mut joined = cleanup.wait();
        assert!(joined.as_mut().now_or_never().is_none());
        drop(first);
        assert_eq!(active.lock().unwrap().requests.len(), 1);
        assert!(joined.as_mut().now_or_never().is_none());
        drop(second);
        joined.await;
        let registry = active.lock().unwrap();
        assert!(registry.requests.is_empty());
        assert!(registry.waiters.is_empty());
    }

    #[test]
    fn discovery_projects_only_versions_and_preserves_independent_backend_fields() {
        let original = json!({
            "supportedVersions": ["2024-11-05", "2025-03-26", "2025-11-25", MCP_VERSION],
            "serverInfo": {"name":"backend","version":"independent-version"},
            "capabilities": {"tools": {"listChanged": true}},
            "instructions": "preserved",
            "_meta": {"vendor/opaque": [null, 42]},
            "extension": {"also":"preserved"}
        });
        let mut expected = original.clone();
        expected["supportedVersions"] = json!([MCP_VERSION]);
        let projected = project_discovery_versions(McpOutcome::Result(original)).unwrap();
        assert_eq!(
            serde_json::to_value(V1McpProtocol::response(projected)).unwrap(),
            json!({"result":expected}),
        );
        #[cfg(feature = "unstable_protocol_v2")]
        assert_eq!(
            serde_json::to_value(V2McpProtocol::response(
                project_discovery_versions(McpOutcome::Result(expected.clone())).unwrap()
            ))
            .unwrap(),
            json!({"result":expected}),
        );
    }

    #[test]
    fn discovery_unsupported_versions_are_inner_errors_but_malformed_results_are_outer() {
        let unsupported = project_discovery_versions(McpOutcome::Result(
            json!({"supportedVersions":["2025-11-25"]}),
        ))
        .unwrap();
        assert_eq!(
            serde_json::to_value(V1McpProtocol::response(unsupported)).unwrap(),
            json!({"error":{
                "code":-32022,"message":"Unsupported protocol version",
                "data":{"requested":MCP_VERSION,"supported":["2025-11-25"]}
            }}),
        );
        for malformed in [
            Value::Null,
            json!({}),
            json!({"supportedVersions":MCP_VERSION}),
            json!({"supportedVersions":[MCP_VERSION, 42]}),
        ] {
            let error = project_discovery_versions(McpOutcome::Result(malformed)).unwrap_err();
            assert_eq!(i32::from(error.code), MCP_BACKEND_FAILURE);
        }
        let error = McpError::new(-32000, "opaque backend error").data(Value::Null);
        let preserved = project_discovery_versions(McpOutcome::Error(error.clone())).unwrap();
        assert_eq!(
            serde_json::to_value(V1McpProtocol::response(preserved)).unwrap(),
            json!({"error":error}),
        );
    }

    #[test]
    fn v1_outcomes_preserve_opaque_results_and_raw_errors() {
        assert_eq!(
            serde_json::to_value(V1McpProtocol::response(McpOutcome::Result(Value::Null))).unwrap(),
            json!({"result":null})
        );
        for data in [None, Some(Value::Null), Some(json!({"cause":"upstream"}))] {
            let raw: RawJsonRpcError = serde_json::from_value({
                let mut value = json!({"code":-32022,"message":"peer","extension":{"retry":false}});
                if let Some(data) = data {
                    value["data"] = data;
                }
                value
            })
            .unwrap();
            let expected = json!({"error":raw});
            assert_eq!(
                serde_json::to_value(V1McpProtocol::response(McpOutcome::Error(into_mcp_error(
                    raw
                ))))
                .unwrap(),
                expected
            );
        }
    }
    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn v2_conversion_is_independent_and_preserves_every_error_field() {
        for data in [None, Some(Value::Null), Some(json!({"cause":"upstream"}))] {
            let mut error = McpError::new(-32000, "peer");
            if let Some(data) = data {
                error = error.data(data);
            }
            error
                .extra
                .insert("extension".into(), json!({"retry":false}));
            let expected = json!({"error":error});
            assert_eq!(
                serde_json::to_value(V2McpProtocol::response(McpOutcome::Error(error))).unwrap(),
                expected
            );
        }
        let value = json!({"resultType":"complete","_meta":{"custom":true}});
        assert_eq!(
            serde_json::to_value(V2McpProtocol::response(McpOutcome::Result(value.clone())))
                .unwrap(),
            json!({"result":value})
        );
    }
    #[test]
    fn request_metadata_requires_modern_version_and_object_capabilities() {
        let params = json!({"_meta":{"io.modelcontextprotocol/protocolVersion":MCP_VERSION,"io.modelcontextprotocol/clientCapabilities":{}}});
        assert!(validate_modern_request("server/discover", params.as_object()).is_ok());
        assert!(validate_modern_request("resources/subscribe", params.as_object()).is_ok());
        assert!(validate_modern_request("initialize", params.as_object()).is_err());
        assert!(validate_modern_request("tools/list", None).is_err());
        let mut invalid = params;
        invalid["_meta"]["io.modelcontextprotocol/clientCapabilities"] = Value::Null;
        assert!(validate_modern_request("tools/list", invalid.as_object()).is_err());
    }
}
