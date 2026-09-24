//! Consume client-provided native MCP servers from an ACP agent connection.
//!
//! [`McpOverAcp::connect_v1`] opens the declared server before returning a
//! standard `ConnectTo<role::mcp::Client>` transport. Keep the transport alive
//! while using it; its [`McpOverAcpClose`] handle permits awaited shutdown from
//! inside the client's connection callback. Dropping the last close handle
//! unregisters routing and schedules a best-effort disconnect.

use std::{
    collections::HashMap,
    marker::PhantomData,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
};
use serde_json::{Map, Value};

use crate::{
    Channel, Client, ConnectTo, ConnectionTo, Dispatch, DynamicHandlerGuard, HandleDispatchFrom,
    Handled, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, UntypedMessage,
    role::{self},
    schema::v1,
    util::MatchDispatchFrom,
};

#[doc(hidden)]
pub trait Wire: Send + Sync + 'static {
    type Connect: JsonRpcRequest<Response = Self::Connected>;
    type Connected: JsonRpcResponse;
    type Request: JsonRpcRequest<Response = Self::Response>;
    type Notification: JsonRpcNotification;
    type Response: JsonRpcResponse;
    type Disconnect: JsonRpcRequest;

    fn connect(id: String) -> Self::Connect;
    fn connected_id(response: Self::Connected) -> String;
    fn request(id: String, method: String, params: Option<Map<String, Value>>) -> Self::Request;
    fn notification(
        id: String,
        method: String,
        params: Option<Map<String, Value>>,
    ) -> Self::Notification;
    fn incoming_request(request: Self::Request) -> (String, String, Option<Map<String, Value>>);
    fn incoming_notification(
        notification: Self::Notification,
    ) -> (String, String, Option<Map<String, Value>>);
    fn disconnect(id: String) -> Self::Disconnect;
}

/// Stable ACP wire version.
#[derive(Debug)]
pub struct V1;
impl Wire for V1 {
    type Connect = v1::ConnectMcpRequest;
    type Connected = v1::ConnectMcpResponse;
    type Request = v1::MessageMcpRequest;
    type Notification = v1::MessageMcpNotification;
    type Response = v1::MessageMcpResponse;
    type Disconnect = v1::DisconnectMcpRequest;

    fn connect(id: String) -> Self::Connect {
        v1::ConnectMcpRequest::new(v1::McpServerAcpId::new(id))
    }
    fn connected_id(response: Self::Connected) -> String {
        response.connection_id.0.to_string()
    }
    fn request(id: String, method: String, params: Option<Map<String, Value>>) -> Self::Request {
        v1::MessageMcpRequest::new(v1::McpConnectionId::new(id), method).params(params)
    }
    fn notification(
        id: String,
        method: String,
        params: Option<Map<String, Value>>,
    ) -> Self::Notification {
        v1::MessageMcpNotification::new(v1::McpConnectionId::new(id), method).params(params)
    }
    fn incoming_request(request: Self::Request) -> (String, String, Option<Map<String, Value>>) {
        (
            request.connection_id.0.to_string(),
            request.method,
            request.params,
        )
    }
    fn incoming_notification(
        notification: Self::Notification,
    ) -> (String, String, Option<Map<String, Value>>) {
        (
            notification.connection_id.0.to_string(),
            notification.method,
            notification.params,
        )
    }
    fn disconnect(id: String) -> Self::Disconnect {
        v1::DisconnectMcpRequest::new(v1::McpConnectionId::new(id))
    }
}

#[cfg(feature = "unstable_protocol_v2")]
/// Draft ACP v2 wire version.
#[derive(Debug)]
pub struct V2;
#[cfg(feature = "unstable_protocol_v2")]
impl Wire for V2 {
    type Connect = crate::schema::v2::ConnectMcpRequest;
    type Connected = crate::schema::v2::ConnectMcpResponse;
    type Request = crate::schema::v2::MessageMcpRequest;
    type Notification = crate::schema::v2::MessageMcpNotification;
    type Response = crate::schema::v2::MessageMcpResponse;
    type Disconnect = crate::schema::v2::DisconnectMcpRequest;

    fn connect(id: String) -> Self::Connect {
        crate::schema::v2::ConnectMcpRequest::new(crate::schema::v2::McpServerAcpId::new(id))
    }
    fn connected_id(response: Self::Connected) -> String {
        response.connection_id.0.to_string()
    }
    fn request(id: String, method: String, params: Option<Map<String, Value>>) -> Self::Request {
        crate::schema::v2::MessageMcpRequest::new(
            crate::schema::v2::McpConnectionId::new(id),
            method,
        )
        .params(params)
    }
    fn notification(
        id: String,
        method: String,
        params: Option<Map<String, Value>>,
    ) -> Self::Notification {
        crate::schema::v2::MessageMcpNotification::new(
            crate::schema::v2::McpConnectionId::new(id),
            method,
        )
        .params(params)
    }
    fn incoming_request(request: Self::Request) -> (String, String, Option<Map<String, Value>>) {
        (
            request.connection_id.0.to_string(),
            request.method,
            request.params,
        )
    }
    fn incoming_notification(
        notification: Self::Notification,
    ) -> (String, String, Option<Map<String, Value>>) {
        (
            notification.connection_id.0.to_string(),
            notification.method,
            notification.params,
        )
    }
    fn disconnect(id: String) -> Self::Disconnect {
        crate::schema::v2::DisconnectMcpRequest::new(crate::schema::v2::McpConnectionId::new(id))
    }
}

fn params(value: Value) -> Result<Option<Map<String, Value>>, crate::Error> {
    match value {
        Value::Object(map) => Ok(Some(map)),
        Value::Null => Ok(None),
        _ => {
            Err(crate::Error::invalid_params()
                .data("native MCP parameters must be an object or null"))
        }
    }
}

struct Incoming<P: Wire> {
    id: Arc<Mutex<Option<String>>>,
    tx: mpsc::UnboundedSender<Dispatch>,
    protocol: PhantomData<P>,
}

impl<P: Wire> HandleDispatchFrom<Client> for Incoming<P> {
    fn describe_chain(&self) -> impl std::fmt::Debug {
        "McpOverAcp"
    }

    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        cx: ConnectionTo<Client>,
    ) -> Result<Handled<Dispatch>, crate::Error> {
        MatchDispatchFrom::new(message, &cx)
            .if_request_from(Client, async |request: P::Request, responder| {
                let (id, method, params) = P::incoming_request(request);
                if self.id.lock().unwrap().as_ref() != Some(&id) {
                    return Ok(Handled::No {
                        message: (P::request(id, method, params), responder),
                        retry: false,
                    });
                }
                let responder = responder.wrap_params(|method, result| {
                    result.and_then(|value: Value| P::Response::from_value(method, value))
                });
                self.tx
                    .unbounded_send(Dispatch::Request(
                        UntypedMessage {
                            method,
                            params: params.map_or(Value::Null, Value::Object),
                        },
                        responder,
                    ))
                    .map_err(crate::Error::into_internal_error)?;
                Ok(Handled::Yes)
            })
            .await
            .if_notification_from(Client, async |notification: P::Notification| {
                let (id, method, params) = P::incoming_notification(notification);
                if self.id.lock().unwrap().as_ref() != Some(&id) {
                    return Ok(Handled::No {
                        message: P::notification(id, method, params),
                        retry: false,
                    });
                }
                self.tx
                    .unbounded_send(Dispatch::Notification(UntypedMessage {
                        method,
                        params: params.map_or(Value::Null, Value::Object),
                    }))
                    .map_err(crate::Error::into_internal_error)?;
                Ok(Handled::Yes)
            })
            .await
            .done()
    }
}

struct Closing<P: Wire> {
    cx: ConnectionTo<Client>,
    id: String,
    guard: Mutex<Option<DynamicHandlerGuard<Client>>>,
    closed: Arc<AtomicBool>,
    disconnected: AtomicBool,
    incoming: mpsc::UnboundedSender<Dispatch>,
    pending: Arc<Mutex<HashMap<u64, crate::Responder<Value>>>>,
    protocol: PhantomData<P>,
}
impl<P: Wire> Closing<P> {
    fn unregister(&self) -> bool {
        let was_closed = self.closed.swap(true, Ordering::AcqRel);
        self.guard.lock().unwrap().take();
        self.incoming.close_channel();
        let outstanding = std::mem::take(&mut *self.pending.lock().unwrap());
        for (_, responder) in outstanding {
            drop(responder.respond_with_error(connection_closed()));
        }
        was_closed
    }
}
fn connection_closed() -> crate::Error {
    crate::Error::internal_error().data("MCP-over-ACP connection closed")
}
impl<P: Wire> Drop for Closing<P> {
    fn drop(&mut self) {
        self.unregister();
        if self.disconnected.load(Ordering::Acquire) {
            return;
        }
        let cx = self.cx.clone();
        let id = self.id.clone();
        // A failed best-effort disconnect must not bring down the ACP connection.
        drop(self.cx.spawn(async move {
            drop(
                cx.send_request_to(Client, P::disconnect(id))
                    .block_task()
                    .await,
            );
            Ok(())
        }));
    }
}

/// Cloneable shutdown handle. Call `close` for an acknowledged disconnect;
/// dropping the final handle only attempts a best-effort disconnect.
pub struct McpOverAcpClose<P: Wire = V1>(Arc<Closing<P>>);
impl<P: Wire> std::fmt::Debug for McpOverAcpClose<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpOverAcpClose")
            .field("id", &self.0.id)
            .finish()
    }
}
impl<P: Wire> Clone for McpOverAcpClose<P> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<P: Wire> McpOverAcpClose<P> {
    /// Unregister callbacks immediately and await the provider's disconnect reply.
    pub async fn close(self) -> Result<(), crate::Error> {
        let already_closed = self.0.unregister();
        if already_closed {
            return Ok(());
        }
        let result = self
            .0
            .cx
            .send_request_to(Client, P::disconnect(self.0.id.clone()))
            .block_task()
            .await
            .map(|_| ());
        if result.is_ok() {
            self.0.disconnected.store(true, Ordering::Release);
        }
        result
    }
}

// Drain queued reverse requests even if the MCP component future is cancelled
// before its incoming task processes them.
struct IncomingQueue(mpsc::UnboundedReceiver<Dispatch>);
impl Drop for IncomingQueue {
    fn drop(&mut self) {
        while let Ok(dispatch) = self.0.try_recv() {
            if let Dispatch::Request(_, responder) = dispatch {
                drop(responder.respond_with_error(connection_closed()));
            }
        }
    }
}

/// An MCP server transport backed by an already-open ACP `mcp/connect`.
///
/// Pass this to `role::mcp::Client.builder().connect_with(...)`. Retain the
/// returned close handle to shut down while the MCP client is running.
pub struct McpOverAcp<P: Wire = V1> {
    cx: ConnectionTo<Client>,
    id: String,
    rx: IncomingQueue,
    close: McpOverAcpClose<P>,
}
impl<P: Wire> std::fmt::Debug for McpOverAcp<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpOverAcp")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl McpOverAcp<V1> {
    /// Connect a v1 `McpServer::Acp` declaration from an ACP agent.
    pub async fn connect_v1(
        cx: &ConnectionTo<Client>,
        id: v1::McpServerAcpId,
    ) -> Result<(Self, McpOverAcpClose<V1>), crate::Error> {
        connect::<V1>(cx, id.0.to_string()).await
    }
}

#[cfg(feature = "unstable_protocol_v2")]
impl McpOverAcp<V2> {
    /// Connect a draft v2 `McpServer::Acp` declaration from an ACP agent.
    pub async fn connect_v2(
        cx: &crate::V2ConnectionTo<Client>,
        id: crate::schema::v2::McpServerAcpId,
    ) -> Result<(Self, McpOverAcpClose<V2>), crate::Error> {
        connect::<V2>(cx.raw_connection(), id.0.to_string()).await
    }
}

async fn connect<P: Wire>(
    cx: &ConnectionTo<Client>,
    server_id: String,
) -> Result<(McpOverAcp<P>, McpOverAcpClose<P>), crate::Error> {
    let (tx, rx) = mpsc::unbounded();
    let id = Arc::new(Mutex::new(None));
    let guard = cx.add_dynamic_handler(Incoming::<P> {
        id: id.clone(),
        tx: tx.clone(),
        protocol: PhantomData,
    })?;
    // The registration is queued asynchronously. Apply it before publishing
    // connect so peer callbacks cannot overtake the handler.
    cx.dynamic_handler_barrier().await?;
    let (reply_tx, reply_rx) = oneshot::channel();
    let reply_cx = cx.clone();
    cx.send_request_to(Client, P::connect(server_id))
        .on_receiving_result(move |reply| {
            let reply = reply.map(P::connected_id);
            if let Ok(connection_id) = &reply {
                // The ACP response callback runs before the next incoming
                // message, even when the peer sends an immediate MCP callback.
                *id.lock().unwrap() = Some(connection_id.clone());
            }
            if let Err(reply) = reply_tx.send(reply) {
                // The caller cancelled while connect was outstanding.
                if let Ok(connection_id) = reply {
                    let cx = reply_cx.clone();
                    drop(reply_cx.spawn(async move {
                        drop(
                            cx.send_request_to(Client, P::disconnect(connection_id))
                                .block_task()
                                .await,
                        );
                        Ok(())
                    }));
                }
            }
            futures::future::ready(Ok(()))
        })?;
    let connection_id = reply_rx
        .await
        .map_err(crate::Error::into_internal_error)??;
    let close = McpOverAcpClose(Arc::new(Closing {
        cx: cx.clone(),
        id: connection_id.clone(),
        guard: Mutex::new(Some(guard)),
        closed: Arc::new(AtomicBool::new(false)),
        disconnected: AtomicBool::new(false),
        incoming: tx,
        pending: Arc::new(Mutex::new(HashMap::new())),
        protocol: PhantomData,
    }));
    Ok((
        McpOverAcp {
            cx: cx.clone(),
            id: connection_id,
            rx: IncomingQueue(rx),
            close: close.clone(),
        },
        close,
    ))
}

impl<P: Wire> ConnectTo<role::mcp::Client> for McpOverAcp<P> {
    async fn connect_to(
        self,
        client: impl ConnectTo<role::mcp::Server>,
    ) -> Result<(), crate::Error> {
        let Self {
            cx,
            id,
            mut rx,
            close,
        } = self;
        let pending = close.0.pending.clone();
        let closed = close.0.closed.clone();
        let next_request = AtomicU64::new(0);
        let (client_channel, server_channel) = Channel::duplex();
        let server = role::mcp::Server
            .builder()
            .on_receive_dispatch(
                async move |message: Dispatch, _mcp_cx| match message {
                    Dispatch::Request(request, responder) => {
                        let (method, value) = request.into_parts();
                        let params = match params(value) {
                            Ok(params) => params,
                            Err(error) => return responder.respond_with_error(error),
                        };
                        let request = P::request(id.clone(), method, params);
                        let responder = responder.wrap_params(|method, result| {
                            result.and_then(|response: P::Response| response.into_json(method))
                        });
                        cx.send_proxied_message_to(
                            Client,
                            Dispatch::<P::Request, P::Notification>::Request(request, responder),
                        )
                    }
                    Dispatch::Notification(notification) => {
                        let (method, value) = notification.into_parts();
                        let params = params(value)?;
                        cx.send_notification_to(Client, P::notification(id.clone(), method, params))
                    }
                    Dispatch::Response(result, router) => router.route_with_result(result),
                },
                crate::on_receive_dispatch!(),
            )
            .with_spawned(move |mcp_cx| async move {
                while let Some(message) = rx.0.next().await {
                    match message {
                        Dispatch::Request(request, responder) => {
                            let mut outstanding = pending.lock().unwrap();
                            if closed.load(Ordering::Acquire) {
                                drop(outstanding);
                                responder.respond_with_error(connection_closed())?;
                                continue;
                            }
                            let sequence = next_request.fetch_add(1, Ordering::Relaxed);
                            outstanding.insert(sequence, responder);
                            drop(outstanding);
                            let waiting = mcp_cx.send_request_to(role::mcp::Client, request);
                            let pending = pending.clone();
                            mcp_cx.spawn(async move {
                                let result = waiting.block_task().await;
                                let responder = pending.lock().unwrap().remove(&sequence);
                                if let Some(responder) = responder {
                                    // The peer may have closed while the MCP request
                                    // was in flight; no error escapes to the ACP loop.
                                    drop(responder.respond_with_result(result));
                                }
                                Ok(())
                            })?;
                        }
                        message => mcp_cx.send_proxied_message_to(role::mcp::Client, message)?,
                    }
                }
                Ok(())
            });
        futures::try_join!(
            server.connect_to(server_channel),
            client.connect_to(client_channel)
        )?;
        Ok(())
    }
}
