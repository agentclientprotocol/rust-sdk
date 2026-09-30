use crate::jsonrpc::{
    ConnectionContext, HandleDispatchFrom, Handled, IntoHandled, JsonRpcResponse,
    RawConnectionContext, connection_context,
};

use crate::role::{HasPeer, Role, handle_incoming_dispatch};
use crate::{ConnectionTo, Dispatch, JsonRpcNotification, JsonRpcRequest, UntypedMessage};
// Types re-exported from crate root
use super::Responder;
use std::future::Future;
use std::marker::PhantomData;
use std::ops::AsyncFnMut;

/// Null handler that accepts no messages.
#[derive(Debug)]
pub struct NullHandler;

impl Default for NullHandler {
    fn default() -> Self {
        Self
    }
}

impl<Counterpart: Role> HandleDispatchFrom<Counterpart> for NullHandler {
    fn describe_chain(&self) -> impl std::fmt::Debug {
        "(null)"
    }

    fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        _cx: ConnectionTo<Counterpart>,
    ) -> impl Future<Output = Result<Handled<Dispatch>, crate::Error>> + Send {
        std::future::ready(Ok(Handled::No {
            message,
            retry: false,
        }))
    }
}

/// Handler for typed request messages
pub struct RequestHandler<
    Counterpart: Role,
    Peer: Role,
    Req: JsonRpcRequest = UntypedMessage,
    F = (),
    ToFut = (),
    Context = RawConnectionContext,
> {
    counterpart: Counterpart,
    peer: Peer,
    handler: F,
    to_future_hack: ToFut,
    phantom: PhantomData<fn(Req, Context)>,
}

impl<Counterpart: Role, Peer: Role, Req: JsonRpcRequest, F, ToFut, Context>
    RequestHandler<Counterpart, Peer, Req, F, ToFut, Context>
{
    /// Creates a new request handler
    pub fn new(counterpart: Counterpart, peer: Peer, handler: F, to_future_hack: ToFut) -> Self {
        Self {
            counterpart,
            peer,
            handler,
            to_future_hack,
            phantom: PhantomData,
        }
    }
}

impl<Counterpart: Role, Peer: Role, Req, F, T, ToFut, Context> HandleDispatchFrom<Counterpart>
    for RequestHandler<Counterpart, Peer, Req, F, ToFut, Context>
where
    Counterpart: HasPeer<Peer>,
    Req: JsonRpcRequest,
    Context: ConnectionContext,
    F: AsyncFnMut(
            Req,
            Responder<Req::Response>,
            Context::Connection<Counterpart>,
        ) -> Result<T, crate::Error>
        + Send,
    T: crate::IntoHandled<(Req, Responder<Req::Response>)>,
    ToFut: Fn(
            &mut F,
            Req,
            Responder<Req::Response>,
            Context::Connection<Counterpart>,
        ) -> crate::BoxFuture<'_, Result<T, crate::Error>>
        + Send
        + Sync,
{
    fn describe_chain(&self) -> impl std::fmt::Debug {
        std::any::type_name::<Req>()
    }

    async fn handle_dispatch_from(
        &mut self,
        dispatch: Dispatch,
        connection: ConnectionTo<Counterpart>,
    ) -> Result<Handled<Dispatch>, crate::Error> {
        handle_incoming_dispatch(
            self.counterpart.clone(),
            self.peer.clone(),
            dispatch,
            connection,
            async |dispatch, connection| {
                match dispatch {
                    Dispatch::Request(message, responder) => {
                        tracing::debug!(
                            request_type = std::any::type_name::<Req>(),
                            method = %message.method,
                            "RequestHandler::handle_request"
                        );
                        if Req::matches_method(&message.method) {
                            match Req::parse_message(&message.method, &message.params) {
                                Ok(req) => {
                                    tracing::trace!(
                                        request_type = std::any::type_name::<Req>(),
                                        "RequestHandler::handle_request: parse completed"
                                    );
                                    let typed_responder = responder.cast();
                                    let result = (self.to_future_hack)(
                                        &mut self.handler,
                                        req,
                                        typed_responder,
                                        connection_context::from_raw::<Context, _>(connection),
                                    )
                                    .await?;
                                    match result.into_handled() {
                                        Handled::Yes => Ok(Handled::Yes),
                                        Handled::No {
                                            message: (request, responder),
                                            retry,
                                        } => {
                                            // Handler returned the request back, convert to untyped
                                            let untyped = request.to_untyped_message()?;
                                            Ok(Handled::No {
                                                message: Dispatch::Request(
                                                    untyped,
                                                    responder.erase_to_json(),
                                                ),
                                                retry,
                                            })
                                        }
                                    }
                                }
                                Err(err) => {
                                    tracing::trace!(
                                        error_code = ?&err.code,
                                        "RequestHandler::handle_request: parse errored"
                                    );
                                    Err(err)
                                }
                            }
                        } else {
                            tracing::trace!("RequestHandler::handle_request: method doesn't match");
                            Ok(Handled::No {
                                message: Dispatch::Request(message, responder),
                                retry: false,
                            })
                        }
                    }

                    Dispatch::Notification(..) | Dispatch::Response(..) => Ok(Handled::No {
                        message: dispatch,
                        retry: false,
                    }),
                }
            },
        )
        .await
    }
}

/// Handler for typed notification messages
pub struct NotificationHandler<
    Counterpart: Role,
    Peer: Role,
    Notif: JsonRpcNotification = UntypedMessage,
    F = (),
    ToFut = (),
    Context = RawConnectionContext,
> {
    counterpart: Counterpart,
    peer: Peer,
    handler: F,
    to_future_hack: ToFut,
    phantom: PhantomData<fn(Notif, Context)>,
}

impl<Counterpart: Role, Peer: Role, Notif: JsonRpcNotification, F, ToFut, Context>
    NotificationHandler<Counterpart, Peer, Notif, F, ToFut, Context>
{
    /// Creates a new notification handler
    pub fn new(counterpart: Counterpart, peer: Peer, handler: F, to_future_hack: ToFut) -> Self {
        Self {
            counterpart,
            peer,
            handler,
            to_future_hack,
            phantom: PhantomData,
        }
    }
}

impl<Counterpart: Role, Peer: Role, Notif, F, T, ToFut, Context> HandleDispatchFrom<Counterpart>
    for NotificationHandler<Counterpart, Peer, Notif, F, ToFut, Context>
where
    Counterpart: HasPeer<Peer>,
    Notif: JsonRpcNotification,
    Context: ConnectionContext,
    F: AsyncFnMut(Notif, Context::Connection<Counterpart>) -> Result<T, crate::Error> + Send,
    T: crate::IntoHandled<(Notif, Context::Connection<Counterpart>)>,
    ToFut: Fn(
            &mut F,
            Notif,
            Context::Connection<Counterpart>,
        ) -> crate::BoxFuture<'_, Result<T, crate::Error>>
        + Send
        + Sync,
{
    fn describe_chain(&self) -> impl std::fmt::Debug {
        std::any::type_name::<Notif>()
    }

    async fn handle_dispatch_from(
        &mut self,
        dispatch: Dispatch,
        connection: ConnectionTo<Counterpart>,
    ) -> Result<Handled<Dispatch>, crate::Error> {
        handle_incoming_dispatch(
            self.counterpart.clone(),
            self.peer.clone(),
            dispatch,
            connection,
            async |dispatch, connection| {
                match dispatch {
                    Dispatch::Notification(message) => {
                        tracing::debug!(
                            request_type = std::any::type_name::<Notif>(),
                            method = %message.method,
                            "NotificationHandler::handle_dispatch"
                        );
                        if Notif::matches_method(&message.method) {
                            match Notif::parse_message(&message.method, &message.params) {
                                Ok(notif) => {
                                    tracing::trace!(
                                        notification_type = std::any::type_name::<Notif>(),
                                        "NotificationHandler::handle_notification: parse completed"
                                    );
                                    let result = (self.to_future_hack)(
                                        &mut self.handler,
                                        notif,
                                        connection_context::from_raw::<Context, _>(connection),
                                    )
                                    .await?;
                                    match result.into_handled() {
                                        Handled::Yes => Ok(Handled::Yes),
                                        Handled::No {
                                            message: (notification, _cx),
                                            retry,
                                        } => {
                                            // Handler returned the notification back, convert to untyped
                                            let untyped = notification.to_untyped_message()?;
                                            Ok(Handled::No {
                                                message: Dispatch::Notification(untyped),
                                                retry,
                                            })
                                        }
                                    }
                                }
                                Err(err) => {
                                    tracing::trace!(
                                        error_code = ?&err.code,
                                        "NotificationHandler::handle_notification: parse errored"
                                    );
                                    Err(err)
                                }
                            }
                        } else {
                            tracing::trace!(
                                "NotificationHandler::handle_notification: method doesn't match"
                            );
                            Ok(Handled::No {
                                message: Dispatch::Notification(message),
                                retry: false,
                            })
                        }
                    }

                    Dispatch::Request(..) | Dispatch::Response(..) => Ok(Handled::No {
                        message: dispatch,
                        retry: false,
                    }),
                }
            },
        )
        .await
    }
}

/// Handler for typed requests, notifications, and matching responses.
pub struct MessageHandler<
    Counterpart: Role,
    Peer: Role,
    Req: JsonRpcRequest = UntypedMessage,
    Notif: JsonRpcNotification = UntypedMessage,
    F = (),
    ToFut = (),
    Context = RawConnectionContext,
> {
    counterpart: Counterpart,
    peer: Peer,
    handler: F,
    to_future_hack: ToFut,
    phantom: PhantomData<fn(Dispatch<Req, Notif>, Context)>,
}

impl<
    Counterpart: Role,
    Peer: Role,
    Req: JsonRpcRequest,
    Notif: JsonRpcNotification,
    F,
    ToFut,
    Context,
> MessageHandler<Counterpart, Peer, Req, Notif, F, ToFut, Context>
{
    /// Creates a new message handler
    pub fn new(counterpart: Counterpart, peer: Peer, handler: F, to_future_hack: ToFut) -> Self {
        Self {
            counterpart,
            peer,
            handler,
            to_future_hack,
            phantom: PhantomData,
        }
    }
}

impl<
    Counterpart: Role,
    Peer: Role,
    Req: JsonRpcRequest,
    Notif: JsonRpcNotification,
    F,
    T,
    ToFut,
    Context,
> HandleDispatchFrom<Counterpart>
    for MessageHandler<Counterpart, Peer, Req, Notif, F, ToFut, Context>
where
    Counterpart: HasPeer<Peer>,
    Context: ConnectionContext,
    F: AsyncFnMut(
            Dispatch<Req, Notif>,
            Context::Connection<Counterpart>,
        ) -> Result<T, crate::Error>
        + Send,
    T: IntoHandled<Dispatch<Req, Notif>>,
    ToFut: Fn(
            &mut F,
            Dispatch<Req, Notif>,
            Context::Connection<Counterpart>,
        ) -> crate::BoxFuture<'_, Result<T, crate::Error>>
        + Send
        + Sync,
{
    fn describe_chain(&self) -> impl std::fmt::Debug {
        format!(
            "({}, {})",
            std::any::type_name::<Req>(),
            std::any::type_name::<Notif>()
        )
    }

    async fn handle_dispatch_from(
        &mut self,
        dispatch: Dispatch,
        connection: ConnectionTo<Counterpart>,
    ) -> Result<Handled<Dispatch>, crate::Error> {
        handle_incoming_dispatch(
            self.counterpart.clone(),
            self.peer.clone(),
            dispatch,
            connection,
            async |dispatch, connection| match dispatch.into_typed_dispatch::<Req, Notif>()? {
                Ok(typed_dispatch) => {
                    let result = (self.to_future_hack)(
                        &mut self.handler,
                        typed_dispatch,
                        connection_context::from_raw::<Context, _>(connection),
                    )
                    .await?;
                    match result.into_handled() {
                        Handled::Yes => Ok(Handled::Yes),
                        Handled::No {
                            message: Dispatch::Request(request, responder),
                            retry,
                        } => {
                            let untyped = request.to_untyped_message()?;
                            Ok(Handled::No {
                                message: Dispatch::Request(untyped, responder.erase_to_json()),
                                retry,
                            })
                        }
                        Handled::No {
                            message: Dispatch::Notification(notification),
                            retry,
                        } => {
                            let untyped = notification.to_untyped_message()?;
                            Ok(Handled::No {
                                message: Dispatch::Notification(untyped),
                                retry,
                            })
                        }
                        Handled::No {
                            message: Dispatch::Response(result, responder),
                            retry,
                        } => {
                            let method = responder.method();
                            let untyped_result = match result {
                                Ok(response) => response.into_json(method).map(Ok),
                                Err(err) => Ok(Err(err)),
                            }?;
                            Ok(Handled::No {
                                message: Dispatch::Response(
                                    untyped_result,
                                    responder.erase_to_json(),
                                ),
                                retry,
                            })
                        }
                    }
                }

                Err(dispatch) => Ok(Handled::No {
                    message: dispatch,
                    retry: false,
                }),
            },
        )
        .await
    }
}

/// Wraps a handler with an optional name for tracing/debugging.
pub struct NamedHandler<H> {
    name: Option<String>,
    handler: H,
}

impl<H> NamedHandler<H> {
    /// Creates a new named handler
    pub fn new(name: Option<String>, handler: H) -> Self {
        Self { name, handler }
    }
}

impl<Counterpart: Role, H: HandleDispatchFrom<Counterpart>> HandleDispatchFrom<Counterpart>
    for NamedHandler<H>
{
    fn describe_chain(&self) -> impl std::fmt::Debug {
        format!(
            "NamedHandler({:?}, {:?})",
            self.name,
            self.handler.describe_chain()
        )
    }

    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        connection: ConnectionTo<Counterpart>,
    ) -> Result<Handled<Dispatch>, crate::Error> {
        if let Some(name) = &self.name {
            crate::util::instrumented_with_connection_name(
                name.clone(),
                self.handler.handle_dispatch_from(message, connection),
            )
            .await
        } else {
            self.handler.handle_dispatch_from(message, connection).await
        }
    }
}

/// Chains two handlers together, trying the first handler and falling back to the second
pub struct ChainedHandler<H1, H2> {
    handler1: H1,
    handler2: H2,
}

impl<H1, H2> ChainedHandler<H1, H2> {
    /// Creates a new chain handler
    pub fn new(handler1: H1, handler2: H2) -> Self {
        Self { handler1, handler2 }
    }
}

/// Heap-allocates a handler's dispatch future; `inline(never)` keeps its
/// construction out of the caller's poll frame.
#[inline(never)]
fn boxed_dispatch<Counterpart, H>(
    handler: &mut H,
    message: Dispatch,
    connection: ConnectionTo<Counterpart>,
) -> crate::BoxFuture<'_, Result<Handled<Dispatch>, crate::Error>>
where
    Counterpart: Role,
    H: HandleDispatchFrom<Counterpart>,
{
    Box::pin(handler.handle_dispatch_from(message, connection))
}

impl<Counterpart: Role, H1, H2> HandleDispatchFrom<Counterpart> for ChainedHandler<H1, H2>
where
    H1: HandleDispatchFrom<Counterpart>,
    H2: HandleDispatchFrom<Counterpart>,
{
    fn describe_chain(&self) -> impl std::fmt::Debug {
        format!(
            "{:?}, {:?}",
            self.handler1.describe_chain(),
            self.handler2.describe_chain()
        )
    }

    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        connection: ConnectionTo<Counterpart>,
    ) -> Result<Handled<Dispatch>, crate::Error> {
        // Box each link so deep chains don't build one giant nested poll
        // frame per handler and overflow small (e.g. GCD 512 KiB) stacks.
        match boxed_dispatch(&mut self.handler1, message, connection.clone()).await? {
            Handled::Yes => Ok(Handled::Yes),
            Handled::No {
                message,
                retry: retry1,
            } => match boxed_dispatch(&mut self.handler2, message, connection).await? {
                Handled::Yes => Ok(Handled::Yes),
                Handled::No {
                    message,
                    retry: retry2,
                } => Ok(Handled::No {
                    message,
                    retry: retry1 | retry2,
                }),
            },
        }
    }
}
