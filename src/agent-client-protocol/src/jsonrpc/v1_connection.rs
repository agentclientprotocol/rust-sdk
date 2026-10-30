//! The protocol-v1 connection type, `agent_client_protocol::v1::ConnectionTo`.

use super::ConnectionTo as RawConnectionTo;
use super::{
    Builder, ConnectTo, ConnectionTermination, Dispatch, DynamicHandlerGuard,
    HandleConnectionClose, HandleDispatchFrom, JsonRpcNotification, JsonRpcRequest,
    PreparedRequest, Protocol, RawJsonRpcId, SentRequest, V1, run::RunWithConnectionTo,
};
use crate::role::{HasPeer, Role};

/// A protocol-v1 connection context.
///
/// Values of this type are supplied to callbacks registered on a
/// [`v1::Builder`](crate::v1::Builder). It exposes the general connection
/// operations, typed for protocol v1, and the v1 session helpers such as
/// [`build_session`](Self::build_session). Requests sent through it return
/// [`v1::SentRequest`](crate::v1::SentRequest) and
/// [`v1::PreparedRequest`](crate::v1::PreparedRequest), whose `id()` is a
/// [`schema::v1::RequestId`](crate::schema::v1::RequestId). The generic
/// JSON-RPC send methods remain schema-agnostic.
///
/// This is a thin, cheaply cloneable handle to the underlying JSON-RPC
/// connection. It intentionally does not implement [`Deref`](std::ops::Deref)
/// to the version-neutral [`ConnectionTo`](crate::ConnectionTo); convert
/// explicitly with [`into_raw`](Self::into_raw) and
/// [`ConnectionTo::into_protocol`](crate::ConnectionTo::into_protocol).
#[derive(Clone, Debug)]
pub struct ConnectionTo<Counterpart: Role> {
    pub(crate) inner: RawConnectionTo<Counterpart>,
}

impl<Counterpart: Role> ConnectionTo<Counterpart> {
    /// Access the underlying version-neutral connection inside the SDK.
    pub(crate) fn raw_connection(&self) -> &RawConnectionTo<Counterpart> {
        &self.inner
    }

    /// The underlying version-neutral connection.
    ///
    /// Requests sent through it return [`Raw`](crate::Raw) handles. Convert
    /// back with [`ConnectionTo::into_protocol`](crate::ConnectionTo::into_protocol).
    #[must_use]
    pub fn into_raw(self) -> RawConnectionTo<Counterpart> {
        self.inner
    }

    /// Return the counterpart role this connection is talking to.
    pub fn counterpart(&self) -> Counterpart {
        self.inner.counterpart()
    }

    /// Wait until the incoming transport reaches clean EOF.
    pub async fn incoming_closed(&self) {
        self.inner.incoming_closed().await;
    }

    /// Return whether clean incoming-EOF processing has completed.
    #[must_use]
    pub fn is_incoming_closed(&self) -> bool {
        self.inner.is_incoming_closed()
    }

    /// Wait until the connection driver has terminated, for any reason.
    ///
    /// See [`ConnectionTo::terminated`](crate::ConnectionTo::terminated).
    pub async fn terminated(&self) -> ConnectionTermination {
        self.inner.terminated().await
    }

    /// Return the termination reason if the connection driver has terminated.
    #[must_use]
    pub fn termination(&self) -> Option<ConnectionTermination> {
        self.inner.termination()
    }

    /// Spawn a task that runs for as long as the JSON-RPC connection is served.
    #[track_caller]
    pub fn spawn(
        &self,
        task: impl IntoFuture<Output = Result<(), crate::Error>, IntoFuture: Send + 'static>,
    ) -> Result<(), crate::Error> {
        self.inner.spawn(task)
    }

    /// Spawn a JSON-RPC connection in the background.
    ///
    /// The returned connection context is selected by `builder`; spawning a
    /// [`v1::Builder`](crate::v1::Builder) therefore returns another
    /// [`v1::ConnectionTo`](ConnectionTo).
    ///
    /// ```no_run
    /// # use agent_client_protocol::{
    /// #     Agent, Client, ConnectTo, Error,
    /// # };
    /// # use agent_client_protocol::v1::ConnectionTo;
    /// # fn example(
    /// #     connection: ConnectionTo<Agent>,
    /// #     transport: impl ConnectTo<Client> + 'static,
    /// # ) -> Result<(), Error> {
    /// let child: ConnectionTo<Agent> =
    ///     connection.spawn_connection(Client.v1(), transport)?;
    /// # drop(child);
    /// # Ok(())
    /// # }
    /// ```
    #[track_caller]
    pub fn spawn_connection<R: Role, P: Protocol>(
        &self,
        builder: Builder<
            R,
            impl HandleDispatchFrom<R::Counterpart> + 'static,
            impl RunWithConnectionTo<R::Counterpart> + 'static,
            impl HandleConnectionClose<R::Counterpart> + 'static,
            P,
        >,
        transport: impl ConnectTo<R> + 'static,
    ) -> Result<P::Connection<R::Counterpart>, crate::Error> {
        self.inner.spawn_connection_with_context(builder, transport)
    }

    /// Send a request or notification and forward its response appropriately.
    pub fn send_proxied_message<
        Req: JsonRpcRequest<Response: Send>,
        Notif: JsonRpcNotification,
        Q: Protocol,
    >(
        &self,
        message: Dispatch<Req, Notif, Q>,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Counterpart>,
    {
        self.inner.send_proxied_message(message)
    }

    /// Send a request or notification to a specific peer and forward its
    /// response appropriately.
    pub fn send_proxied_message_to<
        Peer: Role,
        Req: JsonRpcRequest<Response: Send>,
        Notif: JsonRpcNotification,
        Q: Protocol,
    >(
        &self,
        peer: Peer,
        message: Dispatch<Req, Notif, Q>,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Peer>,
    {
        self.inner.send_proxied_message_to(peer, message)
    }

    /// Send an outgoing request to the default counterpart peer.
    pub fn send_request<Req: JsonRpcRequest>(&self, request: Req) -> SentRequest<Req::Response, V1>
    where
        Counterpart: HasPeer<Counterpart>,
    {
        self.send_request_to(self.inner.counterpart(), request)
    }

    /// Send an outgoing request to a specific peer.
    pub fn send_request_to<Peer: Role, Req: JsonRpcRequest>(
        &self,
        peer: Peer,
        request: Req,
    ) -> SentRequest<Req::Response, V1>
    where
        Counterpart: HasPeer<Peer>,
    {
        self.inner
            .send_request_to_with_options(peer, request, false, None, None)
    }

    /// Prepare a request without sending it until response handling is selected.
    ///
    /// See [`ConnectionTo::prepare_request`](crate::ConnectionTo::prepare_request) for publication and ordering semantics.
    pub fn prepare_request<Req: JsonRpcRequest>(
        &self,
        request: Req,
    ) -> PreparedRequest<Req::Response, V1>
    where
        Counterpart: HasPeer<Counterpart>,
    {
        self.prepare_request_to(self.inner.counterpart(), request)
    }

    /// Prepare a request to a specific peer without sending it.
    ///
    /// See [`ConnectionTo::prepare_request_to`](crate::ConnectionTo::prepare_request_to) for publication and ordering semantics.
    pub fn prepare_request_to<Peer: Role, Req: JsonRpcRequest>(
        &self,
        peer: Peer,
        request: Req,
    ) -> PreparedRequest<Req::Response, V1>
    where
        Counterpart: HasPeer<Peer>,
    {
        self.inner
            .prepare_request_to_with_options(peer, request, None, None)
    }

    /// Send an outgoing notification to the default counterpart peer.
    pub fn send_notification<N: JsonRpcNotification>(
        &self,
        notification: N,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Counterpart>,
    {
        self.inner.send_notification(notification)
    }

    /// Send an outgoing notification to a specific peer.
    pub fn send_notification_to<Peer: Role, N: JsonRpcNotification>(
        &self,
        peer: Peer,
        notification: N,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Peer>,
    {
        self.inner.send_notification_to(peer, notification)
    }

    /// Send a `$/cancel_request` notification to the default counterpart peer.
    pub fn send_cancel_request(
        &self,
        request_id: impl Into<RawJsonRpcId>,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Counterpart>,
    {
        self.inner.send_cancel_request(request_id)
    }

    /// Send a `$/cancel_request` notification to a specific peer.
    pub fn send_cancel_request_to<Peer: Role>(
        &self,
        peer: Peer,
        request_id: impl Into<RawJsonRpcId>,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Peer>,
    {
        self.inner.send_cancel_request_to(peer, request_id)
    }

    /// Register a low-level dynamic message handler.
    ///
    /// Dynamic handlers use the version-neutral [`HandleDispatchFrom`] trait,
    /// so their callback receives the underlying version-neutral
    /// [`ConnectionTo`](crate::ConnectionTo). Prefer typed handlers on
    /// [`v1::Builder`](crate::v1::Builder) when registration can happen before the
    /// connection starts.
    ///
    /// ```no_run
    /// # use agent_client_protocol::{
    /// #     Agent, Client, ConnectTo, DynamicHandlerGuard, Error, NullHandler,
    /// # };
    /// # async fn example(
    /// #     transport: impl ConnectTo<Client> + 'static,
    /// # ) -> Result<(), Error> {
    /// Client.v1().connect_with(transport, async |connection| {
    ///     let guard: DynamicHandlerGuard<Agent> =
    ///         connection.add_dynamic_handler(NullHandler)?;
    ///
    ///     // Keep `guard` alive for as long as the modal handler is needed.
    ///     drop(guard);
    ///     Ok(())
    /// }).await
    /// # }
    /// ```
    pub fn add_dynamic_handler(
        &self,
        handler: impl HandleDispatchFrom<Counterpart> + 'static,
    ) -> Result<DynamicHandlerGuard<Counterpart>, crate::Error> {
        self.inner.add_dynamic_handler(handler)
    }
}
