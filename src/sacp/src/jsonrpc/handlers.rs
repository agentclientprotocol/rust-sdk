use crate::jsonrpc::{Handled, JrMessageHandler};
use crate::{JrConnectionCx, JrNotification, JrRequest, MessageAndCx};
// Types re-exported from crate root
use super::JrRequestCx;
use std::marker::PhantomData;
use std::ops::AsyncFnMut;

/// Null handler that accepts no messages.
#[derive(Default)]
pub struct NullHandler {
    _private: (),
}

impl JrMessageHandler for NullHandler {
    fn describe_chain(&self) -> impl std::fmt::Debug {
        "(null)"
    }

    async fn handle_message(
        &mut self,
        message: MessageAndCx,
    ) -> Result<Handled<MessageAndCx>, crate::Error> {
        Ok(Handled::No(message))
    }
}

/// Handler for typed request messages
pub struct RequestHandler<R, F>
where
    R: JrRequest,
{
    handler: F,
    phantom: PhantomData<fn(R)>,
}

impl<R, F> RequestHandler<R, F>
where
    R: JrRequest,
{
    /// Creates a new request handler
    pub fn new(handler: F) -> Self {
        Self {
            handler,
            phantom: PhantomData,
        }
    }
}

impl<R, F, T> JrMessageHandler for RequestHandler<R, F>
where
    R: JrRequest,
    F: AsyncFnMut(R, JrRequestCx<R::Response>) -> Result<T, crate::Error>,
    T: crate::IntoHandled<(R, JrRequestCx<R::Response>)>,
{
    async fn handle_message(
        &mut self,
        message_cx: MessageAndCx,
    ) -> Result<Handled<MessageAndCx>, crate::Error> {
        match message_cx {
            MessageAndCx::Request(message, request_cx) => {
                tracing::debug!(
                    request_type = std::any::type_name::<R>(),
                    message = ?message,
                    "RequestHandler::handle_request"
                );
                match R::parse_request(&message.method, &message.params) {
                    Some(Ok(req)) => {
                        tracing::trace!(?req, "RequestHandler::handle_request: parse completed");
                        let typed_request_cx = request_cx.cast();
                        let result = (self.handler)(req, typed_request_cx).await?;
                        match result.into_handled() {
                            Handled::Yes => Ok(Handled::Yes),
                            Handled::No((request, request_cx)) => {
                                // Handler returned the request back, convert to untyped
                                let untyped = request.into_untyped_message()?;
                                Ok(Handled::No(MessageAndCx::Request(
                                    untyped,
                                    request_cx.erase_to_json(),
                                )))
                            }
                        }
                    }
                    Some(Err(err)) => {
                        tracing::trace!(?err, "RequestHandler::handle_request: parse errored");
                        Err(err)
                    }
                    None => {
                        tracing::trace!("RequestHandler::handle_request: parse failed");
                        Ok(Handled::No(MessageAndCx::Request(message, request_cx)))
                    }
                }
            }

            MessageAndCx::Notification(..) => Ok(Handled::No(message_cx)),
        }
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        std::any::type_name::<R>()
    }
}

/// Handler for typed notification messages
pub struct NotificationHandler<N, F>
where
    N: JrNotification,
{
    handler: F,
    phantom: PhantomData<fn(N)>,
}

impl<R, F> NotificationHandler<R, F>
where
    R: JrNotification,
{
    /// Creates a new notification handler
    pub fn new(handler: F) -> Self {
        Self {
            handler,
            phantom: PhantomData,
        }
    }
}

impl<R, F, T> JrMessageHandler for NotificationHandler<R, F>
where
    R: JrNotification,
    F: AsyncFnMut(R, JrConnectionCx) -> Result<T, crate::Error>,
    T: crate::IntoHandled<(R, JrConnectionCx)>,
{
    async fn handle_message(
        &mut self,
        message_cx: MessageAndCx,
    ) -> Result<Handled<MessageAndCx>, crate::Error> {
        match message_cx {
            MessageAndCx::Notification(message, cx) => {
                tracing::debug!(
                    request_type = std::any::type_name::<R>(),
                    message = ?message,
                    "NotificationHandler::handle_message"
                );
                match R::parse_notification(&message.method, &message.params) {
                    Some(Ok(notif)) => {
                        tracing::trace!(
                            ?notif,
                            "NotificationHandler::handle_notification: parse completed"
                        );
                        let result = (self.handler)(notif, cx.clone()).await?;
                        match result.into_handled() {
                            Handled::Yes => Ok(Handled::Yes),
                            Handled::No((notification, cx)) => {
                                // Handler returned the notification back, convert to untyped
                                let untyped = notification.into_untyped_message()?;
                                Ok(Handled::No(MessageAndCx::Notification(untyped, cx)))
                            }
                        }
                    }
                    Some(Err(err)) => {
                        tracing::trace!(
                            ?err,
                            "NotificationHandler::handle_notification: parse errored"
                        );
                        Err(err)
                    }
                    None => {
                        tracing::trace!("NotificationHandler::handle_notification: parse failed");
                        Ok(Handled::No(MessageAndCx::Notification(message, cx)))
                    }
                }
            }

            MessageAndCx::Request(..) => Ok(Handled::No(message_cx)),
        }
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        std::any::type_name::<R>()
    }
}

/// Handler that tries H1 and then H2.

pub struct MessageHandler<R, N, F>
where
    R: JrRequest,
    N: JrNotification,
    F: AsyncFnMut(MessageAndCx<R, N>) -> Result<(), crate::Error>,
{
    handler: F,
    phantom: PhantomData<fn(R, N)>,
}

impl<R, N, F> MessageHandler<R, N, F>
where
    R: JrRequest,
    N: JrNotification,
    F: AsyncFnMut(MessageAndCx<R, N>) -> Result<(), crate::Error>,
{
    /// Creates a new message handler
    pub fn new(handler: F) -> Self {
        Self {
            handler,
            phantom: PhantomData,
        }
    }
}

impl<R, N, F> JrMessageHandler for MessageHandler<R, N, F>
where
    R: JrRequest,
    N: JrNotification,
    F: AsyncFnMut(MessageAndCx<R, N>) -> Result<(), crate::Error>,
{
    async fn handle_message(
        &mut self,
        message_cx: MessageAndCx,
    ) -> Result<Handled<MessageAndCx>, crate::Error> {
        match message_cx {
            MessageAndCx::Request(message, request_cx) => {
                tracing::debug!(
                    request_type = std::any::type_name::<R>(),
                    message = ?message,
                    "MessageHandler::handle_request"
                );
                match R::parse_request(&message.method, &message.params) {
                    Some(Ok(req)) => {
                        tracing::trace!(?req, "MessageHandler::handle_request: parse completed");
                        let typed_message = MessageAndCx::Request(req, request_cx.cast());
                        (self.handler)(typed_message).await?;
                        Ok(Handled::Yes)
                    }
                    Some(Err(err)) => {
                        tracing::trace!(?err, "MessageHandler::handle_request: parse errored");
                        Err(err)
                    }
                    None => {
                        tracing::trace!("MessageHandler::handle_request: parse failed");
                        Ok(Handled::No(MessageAndCx::Request(message, request_cx)))
                    }
                }
            }

            MessageAndCx::Notification(message, cx) => {
                tracing::debug!(
                    notification_type = std::any::type_name::<N>(),
                    message = ?message,
                    "MessageHandler::handle_notification"
                );
                match N::parse_notification(&message.method, &message.params) {
                    Some(Ok(notif)) => {
                        tracing::trace!(
                            ?notif,
                            "MessageHandler::handle_notification: parse completed"
                        );
                        let typed_message = MessageAndCx::Notification(notif, cx);
                        (self.handler)(typed_message).await?;
                        Ok(Handled::Yes)
                    }
                    Some(Err(err)) => {
                        tracing::trace!(?err, "MessageHandler::handle_notification: parse errored");
                        Err(err)
                    }
                    None => {
                        tracing::trace!("MessageHandler::handle_notification: parse failed");
                        Ok(Handled::No(MessageAndCx::Notification(message, cx)))
                    }
                }
            }
        }
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        format!(
            "({}, {})",
            std::any::type_name::<R>(),
            std::any::type_name::<N>()
        )
    }
}

/// Chains two handlers together, trying the first handler and falling back to the second
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

impl<H> JrMessageHandler for NamedHandler<H>
where
    H: JrMessageHandler,
{
    fn describe_chain(&self) -> impl std::fmt::Debug {
        format!(
            "NamedHandler({:?}, {:?})",
            self.name,
            self.handler.describe_chain()
        )
    }

    async fn handle_message(
        &mut self,
        message: MessageAndCx,
    ) -> Result<Handled<MessageAndCx>, crate::Error> {
        if let Some(name) = &self.name {
            crate::util::instrumented_with_connection_name(
                name.clone(),
                self.handler.handle_message(message),
            )
            .await
        } else {
            self.handler.handle_message(message).await
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

impl<H1, H2> JrMessageHandler for ChainedHandler<H1, H2>
where
    H1: JrMessageHandler,
    H2: JrMessageHandler,
{
    fn describe_chain(&self) -> impl std::fmt::Debug {
        return DebugImpl {
            handler1: &self.handler1,
            handler2: &self.handler2,
        };

        struct DebugImpl<'h, H1, H2> {
            handler1: &'h H1,
            handler2: &'h H2,
        }

        impl<H1: JrMessageHandler, H2: JrMessageHandler> std::fmt::Debug for DebugImpl<'_, H1, H2> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(
                    f,
                    "{:?}, {:?}",
                    self.handler1.describe_chain(),
                    self.handler2.describe_chain()
                )
            }
        }
    }

    async fn handle_message(
        &mut self,
        message: MessageAndCx,
    ) -> Result<Handled<MessageAndCx>, crate::Error> {
        match self.handler1.handle_message(message).await? {
            Handled::Yes => Ok(Handled::Yes),
            Handled::No(message) => self.handler2.handle_message(message).await,
        }
    }
}
