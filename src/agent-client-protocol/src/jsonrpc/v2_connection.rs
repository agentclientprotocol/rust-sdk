//! The protocol-v2 connection type, `agent_client_protocol::v2::ConnectionTo`.

use super::ConnectionTo as RawConnectionTo;
use super::{
    Builder, ConnectTo, ConnectionTermination, Dispatch, DynamicHandlerGuard,
    HandleConnectionClose, HandleDispatchFrom, JsonRpcNotification, JsonRpcRequest,
    PreparedRequest, Protocol, RawJsonRpcId, SentRequest, V2, protocol_compat,
    run::RunWithConnectionTo,
};
use crate::Agent;
use crate::role::{HasPeer, Role};
use crate::v2::InitializeOutcome;

/// A protocol-v2 connection context.
///
/// Values of this type are supplied to callbacks registered on a
/// [`v2::Builder`](crate::v2::Builder). It exposes the general connection operations that are valid
/// for protocol v2 while keeping version-specific high-level helpers for other
/// protocol versions out of the typed context. The generic JSON-RPC send
/// methods remain intentionally schema-agnostic.
///
/// This is a thin, cheaply cloneable handle to the underlying JSON-RPC
/// connection. It intentionally does not implement [`Deref`](std::ops::Deref)
/// to the version-neutral [`ConnectionTo`](crate::ConnectionTo).
#[derive(Clone, Debug)]
pub struct ConnectionTo<Counterpart: Role> {
    pub(crate) inner: RawConnectionTo<Counterpart>,
}

impl ConnectionTo<Agent> {
    /// Send the client's `initialize` request and report how the agent
    /// negotiated.
    ///
    /// Unlike [`send_request`](Self::send_request), a v1-only agent's valid v1
    /// response resolves successfully as [`InitializeOutcome::LowerVersion`]
    /// with the agent's typed response, so the caller can decide whether to
    /// fall back to a v1 client implementation. The connection stays
    /// uninitialized in that case; the SDK never converts it to v1.
    ///
    /// The request still resolves with an error for errors returned by the
    /// agent (including the `-32022` version mismatch), malformed responses,
    /// and transport failures.
    ///
    /// After an initialization completes, `initialize` may be sent again
    /// before any other request; the agent treats the latest one as
    /// authoritative. Once another request has been sent, a repeat resolves
    /// with an `-32600` error without being sent.
    ///
    /// ```no_run
    /// # use agent_client_protocol::schema::{ProtocolVersion, v2};
    /// # use agent_client_protocol::{Client, ConnectTo};
    /// use agent_client_protocol::v2::{InitializeOutcome, SelectedInitializeResponse};
    /// # async fn example(agent: impl ConnectTo<Client>) -> agent_client_protocol::Result<()> {
    /// let outcome = Client
    ///     .v2()
    ///     .connect_with(agent, async |cx| {
    ///         let request = v2::InitializeRequest::new(
    ///             ProtocolVersion::V2,
    ///             v2::Implementation::new("example", "0.1.0"),
    ///         );
    ///         match cx.send_initialize(request).block_task().await? {
    ///             InitializeOutcome::Initialized(_response) => {
    ///                 // Continue with v2 traffic.
    ///                 Ok(None)
    ///             }
    ///             InitializeOutcome::LowerVersion(mismatch) => Ok(Some(mismatch)),
    ///             _ => Err(agent_client_protocol::util::internal_error("unexpected outcome")),
    ///         }
    ///     })
    ///     .await?;
    /// if let Some(mismatch) = outcome
    ///     && let SelectedInitializeResponse::V1(_v1_response) = mismatch.response
    /// {
    ///     // Reconnect with a v1 client implementation.
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn send_initialize(
        &self,
        request: crate::schema::v2::InitializeRequest,
    ) -> SentRequest<InitializeOutcome, V2> {
        let mut prepared = self.inner.prepare_request_to_with_options::<V2, _, _>(
            self.inner.counterpart(),
            protocol_compat::InitializeNegotiation(request),
            None,
            None,
        );
        prepared
            .publication
            .pending_reply
            .accepts_lower_initialize_version = true;
        prepared.into_sent_request(false)
    }
}

impl<Counterpart: Role> ConnectionTo<Counterpart> {
    /// Access the underlying version-neutral connection inside the SDK.
    pub(crate) fn raw_connection(&self) -> &RawConnectionTo<Counterpart> {
        &self.inner
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
    /// [`v2::Builder`](crate::v2::Builder) therefore returns another
    /// [`v2::ConnectionTo`](ConnectionTo).
    ///
    /// ```no_run
    /// # use agent_client_protocol::{
    /// #     Agent, Client, ConnectTo, Error,
    /// # };
    /// # use agent_client_protocol::v2::ConnectionTo;
    /// # fn example(
    /// #     connection: ConnectionTo<Agent>,
    /// #     transport: impl ConnectTo<Client> + 'static,
    /// # ) -> Result<(), Error> {
    /// let child: ConnectionTo<Agent> =
    ///     connection.spawn_connection(Client.v2(), transport)?;
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
    pub fn send_request<Req: JsonRpcRequest>(&self, request: Req) -> SentRequest<Req::Response, V2>
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
    ) -> SentRequest<Req::Response, V2>
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
    ) -> PreparedRequest<Req::Response, V2>
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
    ) -> PreparedRequest<Req::Response, V2>
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
    /// [`v2::Builder`](crate::v2::Builder) when registration can happen before the
    /// connection starts.
    ///
    /// ```no_run
    /// # use agent_client_protocol::{
    /// #     Agent, Client, ConnectTo, DynamicHandlerGuard, Error, NullHandler,
    /// # };
    /// # async fn example(
    /// #     transport: impl ConnectTo<Client> + 'static,
    /// # ) -> Result<(), Error> {
    /// Client.v2().connect_with(transport, async |connection| {
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
