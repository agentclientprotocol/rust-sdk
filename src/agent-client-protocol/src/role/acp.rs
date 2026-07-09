use std::{fmt::Debug, hash::Hash};

#[cfg(feature = "unstable_protocol_v2")]
use futures::{StreamExt as _, future};

#[cfg(feature = "unstable_protocol_v2")]
use crate::DynConnectTo;
use crate::jsonrpc::{Builder, handlers::NullHandler, run::NullRun};
use crate::role::{HasPeer, RemoteStyle};
#[cfg(feature = "unstable_protocol_v2")]
use crate::schema::ProtocolVersion;
#[cfg(feature = "unstable_protocol_v2")]
use crate::schema::v1::RequestId;
use crate::schema::v1::{InitializeRequest, NewSessionRequest, NewSessionResponse, SessionId};
use crate::schema::{InitializeProxyRequest, METHOD_INITIALIZE_PROXY};
use crate::util::MatchDispatchFrom;
#[cfg(feature = "unstable_protocol_v2")]
use crate::{Channel, RawJsonRpcMessage, RawJsonRpcParams};
use crate::{ConnectTo, ConnectionTo, Dispatch, HandleDispatchFrom, Handled, Role, RoleId};

/// The client role - typically an IDE or CLI that controls an agent.
///
/// Clients send prompts and receive responses from agents.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Client;

impl Role for Client {
    type Counterpart = Agent;

    fn builder(self) -> Builder<Self> {
        Builder::new(self).v1_client()
    }

    async fn default_handle_dispatch_from(
        &self,
        message: Dispatch,
        _connection: ConnectionTo<Client>,
    ) -> Result<Handled<Dispatch>, crate::Error> {
        Ok(Handled::No {
            message,
            retry: false,
        })
    }

    fn role_id(&self) -> RoleId {
        RoleId::from_singleton(self)
    }

    fn counterpart(&self) -> Self::Counterpart {
        Agent
    }
}

impl Client {
    /// Create a connection builder for a client.
    pub fn builder(self) -> Builder<Client, NullHandler, NullRun> {
        <Self as Role>::builder(self)
    }

    /// Create a client builder that requires an ACP protocol v2 agent.
    ///
    /// If the agent negotiates v1 during initialization, the initialize
    /// request resolves with an error so callers can choose an explicit v1
    /// fallback path.
    ///
    /// Requires the `unstable_protocol_v2` crate feature.
    #[cfg(feature = "unstable_protocol_v2")]
    pub fn v2(self) -> Builder<Client, NullHandler, NullRun> {
        self.builder().v2_client()
    }

    /// Connect to `agent` and run `main_fn` with the [`ConnectionTo`].
    /// Returns the result of `main_fn` (or an error if something goes wrong).
    ///
    /// Equivalent to `self.builder().connect_with(agent, main_fn)`.
    pub async fn connect_with<R>(
        self,
        agent: impl ConnectTo<Client>,
        main_fn: impl AsyncFnOnce(ConnectionTo<Agent>) -> Result<R, crate::Error>,
    ) -> Result<R, crate::Error> {
        self.builder().connect_with(agent, main_fn).await
    }
}

impl HasPeer<Client> for Client {
    fn remote_style(&self, _peer: Client) -> RemoteStyle {
        RemoteStyle::Counterpart
    }
}

/// The agent role - typically an LLM that responds to prompts.
///
/// Agents receive prompts from clients and respond with answers,
/// potentially invoking tools along the way.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Agent;

impl Role for Agent {
    type Counterpart = Client;

    fn builder(self) -> Builder<Self> {
        Builder::new(self).v1_agent()
    }

    fn role_id(&self) -> RoleId {
        RoleId::from_singleton(self)
    }

    fn counterpart(&self) -> Self::Counterpart {
        Client
    }

    async fn default_handle_dispatch_from(
        &self,
        message: Dispatch,
        connection: ConnectionTo<Agent>,
    ) -> Result<Handled<Dispatch>, crate::Error> {
        MatchDispatchFrom::new(message, &connection)
            .if_message_from(Agent, async |message: Dispatch| {
                // Subtle: messages that have a session-id field
                // should be captured by a dynamic message handler
                // for that session -- but there is a race condition
                // between the dynamic handler being added and
                // possible updates. Therefore, we "retry" all such
                // messages, so that they will be resent as new handlers
                // are added.
                let retry = message.has_session_id();
                Ok(Handled::No { message, retry })
            })
            .await
            .done()
    }
}

impl Agent {
    /// Create a connection builder for an agent.
    pub fn builder(self) -> Builder<Agent, NullHandler, NullRun> {
        <Self as Role>::builder(self)
    }

    /// Create an agent builder that uses the ACP protocol v2 API.
    ///
    /// This builder requires clients to negotiate protocol v2 during
    /// initialization. Use a v1 builder for v1 clients.
    ///
    /// Requires the `unstable_protocol_v2` crate feature.
    #[cfg(feature = "unstable_protocol_v2")]
    pub fn v2(self) -> Builder<Agent, NullHandler, NullRun> {
        self.builder().v2_agent()
    }

    /// Create a router that chooses between configured protocol implementations.
    ///
    /// Add implementations with [`AgentProtocolRouter::with_v1`] and
    /// [`AgentProtocolRouter::with_v2`].
    /// The resulting router reads the initial
    /// `initialize` request, selects the highest configured implementation
    /// compatible with the client's requested protocol version, then forwards
    /// the connection to that implementation. It does not convert traffic
    /// between protocol versions after routing.
    ///
    /// Requires the `unstable_protocol_v2` crate feature while protocol v2
    /// stabilizes.
    #[cfg(feature = "unstable_protocol_v2")]
    #[must_use]
    pub fn protocol_router(self) -> AgentProtocolRouter {
        AgentProtocolRouter::new()
    }
}

/// Agent component that routes each connection to a configured protocol implementation.
///
/// Use [`Agent::protocol_router`] to start the builder, then add each supported
/// protocol version independently. The selected implementation owns the
/// connection after the initial `initialize` negotiation.
#[cfg(feature = "unstable_protocol_v2")]
#[derive(Debug, Default)]
pub struct AgentProtocolRouter {
    v1: Option<DynConnectTo<Client>>,
    v2: Option<DynConnectTo<Client>>,
}

#[cfg(feature = "unstable_protocol_v2")]
impl AgentProtocolRouter {
    /// Create an empty agent protocol router.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return this router with an ACP v1 implementation configured.
    #[must_use]
    pub fn with_v1(mut self, agent: impl ConnectTo<Client>) -> Self {
        self.v1 = Some(DynConnectTo::new(agent));
        self
    }

    /// Return this router with an ACP v2 implementation configured.
    #[must_use]
    pub fn with_v2(mut self, agent: impl ConnectTo<Client>) -> Self {
        self.v2 = Some(DynConnectTo::new(agent));
        self
    }
}

#[cfg(feature = "unstable_protocol_v2")]
impl ConnectTo<Client> for AgentProtocolRouter {
    async fn connect_to(self, client: impl ConnectTo<Agent>) -> Result<(), crate::Error> {
        let (mut client_channel, client_future) = client.into_channel_and_future();
        let (first_message, client_future): (
            Result<RawJsonRpcMessage, crate::Error>,
            crate::BoxFuture<'static, Result<(), crate::Error>>,
        ) = match future::select(Box::pin(client_channel.rx.next()), client_future).await {
            future::Either::Left((Some(first_message), client_future)) => {
                (first_message, client_future)
            }
            future::Either::Left((None, client_future)) => return client_future.await,
            future::Either::Right((result, first_message)) => {
                result?;
                drop(first_message);
                let Some(first_message) = client_channel.rx.next().await else {
                    return Ok(());
                };
                (first_message, Box::pin(future::ready(Ok(()))))
            }
        };

        let mut first_message = first_message?;
        let supported = SupportedAgentProtocols {
            v1: self.v1.is_some(),
            v2: self.v2.is_some(),
        };
        let selected = match select_agent_protocol(&mut first_message, supported) {
            Ok(selected) => selected,
            Err(error) => {
                send_initialize_error(&client_channel.tx, &first_message, error.clone())?;
                return Err(error);
            }
        };
        let Some(agent) = selected.take_agent(self) else {
            let error = selected.unsupported_error(supported);
            send_initialize_error(&client_channel.tx, &first_message, error.clone())?;
            return Err(error);
        };

        let (router_channel, agent_channel) = Channel::duplex();
        let Channel {
            rx: from_agent,
            tx: to_agent,
        } = router_channel;

        to_agent
            .unbounded_send(Ok(first_message))
            .map_err(crate::util::internal_error)?;

        let agent_future = Box::pin(agent.connect_to(agent_channel));

        let ((), (), (), ()) = futures::try_join!(
            client_future,
            agent_future,
            Channel {
                rx: client_channel.rx,
                tx: to_agent,
            }
            .copy(),
            Channel {
                rx: from_agent,
                tx: client_channel.tx,
            }
            .copy(),
        )?;

        Ok(())
    }
}

#[cfg(feature = "unstable_protocol_v2")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentProtocol {
    V1,
    V2,
}

#[cfg(feature = "unstable_protocol_v2")]
impl AgentProtocol {
    fn version(self) -> ProtocolVersion {
        match self {
            Self::V1 => ProtocolVersion::V1,
            Self::V2 => ProtocolVersion::V2,
        }
    }

    fn take_agent(self, agent: AgentProtocolRouter) -> Option<DynConnectTo<Client>> {
        match self {
            Self::V1 => agent.v1,
            Self::V2 => agent.v2,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::V1 => "1",
            Self::V2 => "2",
        }
    }

    fn unsupported_error(self, supported: SupportedAgentProtocols) -> crate::Error {
        crate::Error::invalid_request().data(format!(
            "ACP protocol version {} is not configured; this endpoint supports {}",
            self.name(),
            supported.description()
        ))
    }
}

#[cfg(feature = "unstable_protocol_v2")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SupportedAgentProtocols {
    v1: bool,
    v2: bool,
}

#[cfg(feature = "unstable_protocol_v2")]
impl SupportedAgentProtocols {
    fn highest_compatible(self, requested: ProtocolVersion) -> Option<AgentProtocol> {
        if self.v2 && requested >= ProtocolVersion::V2 {
            return Some(AgentProtocol::V2);
        }

        if self.v1 && requested >= ProtocolVersion::V1 {
            return Some(AgentProtocol::V1);
        }

        None
    }

    fn description(self) -> String {
        match (self.v1, self.v2) {
            (true, true) => "ACP protocol versions 1 and 2".into(),
            (true, false) => "ACP protocol version 1".into(),
            (false, true) => "ACP protocol version 2".into(),
            (false, false) => "no ACP protocol versions".into(),
        }
    }
}

#[cfg(feature = "unstable_protocol_v2")]
fn select_agent_protocol(
    message: &mut RawJsonRpcMessage,
    supported: SupportedAgentProtocols,
) -> Result<AgentProtocol, crate::Error> {
    let RawJsonRpcMessage::Request(request) = message else {
        return Err(
            crate::Error::invalid_request().data("first ACP message must be an initialize request")
        );
    };

    if request.method.as_ref() != "initialize" {
        return Err(crate::Error::invalid_request().data("first ACP request must be initialize"));
    }

    let Some(RawJsonRpcParams::Object(params)) = &mut request.params else {
        return Err(invalid_initialize_protocol_version());
    };
    let Some(protocol_version) = params.get("protocolVersion") else {
        return Err(invalid_initialize_protocol_version());
    };

    let requested = serde_json::from_value::<ProtocolVersion>(protocol_version.clone())
        .map_err(|_| invalid_initialize_protocol_version())?;
    let selected = highest_compatible_agent_protocol(requested, supported)?;
    params.insert(
        "protocolVersion".into(),
        serde_json::to_value(selected.version()).map_err(crate::Error::into_internal_error)?,
    );

    Ok(selected)
}

#[cfg(feature = "unstable_protocol_v2")]
fn highest_compatible_agent_protocol(
    requested: ProtocolVersion,
    supported: SupportedAgentProtocols,
) -> Result<AgentProtocol, crate::Error> {
    supported.highest_compatible(requested).ok_or_else(|| {
        crate::Error::invalid_request().data(format!(
            "unsupported ACP protocol version {requested}; this endpoint supports {}",
            supported.description()
        ))
    })
}

#[cfg(feature = "unstable_protocol_v2")]
fn invalid_initialize_protocol_version() -> crate::Error {
    crate::Error::invalid_params()
        .data("initialize.protocolVersion must be a valid ACP protocol version")
}

#[cfg(feature = "unstable_protocol_v2")]
fn send_initialize_error(
    tx: &futures::channel::mpsc::UnboundedSender<Result<RawJsonRpcMessage, crate::Error>>,
    message: &RawJsonRpcMessage,
    error: crate::Error,
) -> Result<(), crate::Error> {
    let id = match message {
        RawJsonRpcMessage::Request(request) => request.id.clone(),
        RawJsonRpcMessage::Notification(_) | RawJsonRpcMessage::Response(_) => RequestId::Null,
    };
    tx.unbounded_send(Ok(RawJsonRpcMessage::response(id, Err(error))))
        .map_err(crate::util::internal_error)
}

impl HasPeer<Agent> for Agent {
    fn remote_style(&self, _peer: Agent) -> RemoteStyle {
        RemoteStyle::Counterpart
    }
}

/// The proxy role - an intermediary that can intercept and modify messages.
///
/// Proxies sit between a client and an agent (or another proxy), and can:
/// - Add tools via MCP servers
/// - Filter or transform messages
/// - Inject additional context
///
/// Proxies connect to a [`Conductor`] which orchestrates the proxy chain.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Proxy;

impl Role for Proxy {
    type Counterpart = Conductor;

    async fn default_handle_dispatch_from(
        &self,
        message: crate::Dispatch,
        _connection: crate::ConnectionTo<Self>,
    ) -> Result<crate::Handled<crate::Dispatch>, crate::Error> {
        Ok(Handled::No {
            message,
            retry: false,
        })
    }

    fn role_id(&self) -> RoleId {
        RoleId::from_singleton(self)
    }

    fn counterpart(&self) -> Self::Counterpart {
        Conductor
    }
}

impl Proxy {
    /// Create a connection builder for a proxy.
    pub fn builder(self) -> Builder<Proxy, NullHandler, NullRun> {
        Builder::new(self)
    }
}

impl HasPeer<Proxy> for Proxy {
    fn remote_style(&self, _peer: Proxy) -> RemoteStyle {
        RemoteStyle::Counterpart
    }
}

/// The conductor role - orchestrates proxy chains.
///
/// Conductors manage connections between clients, proxies, and agents,
/// routing messages through the appropriate proxy chain.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Conductor;

impl Role for Conductor {
    type Counterpart = Proxy;

    fn role_id(&self) -> RoleId {
        RoleId::from_singleton(self)
    }

    fn counterpart(&self) -> Self::Counterpart {
        Proxy
    }

    async fn default_handle_dispatch_from(
        &self,
        message: Dispatch,
        cx: ConnectionTo<Conductor>,
    ) -> Result<Handled<Dispatch>, crate::Error> {
        // Handle various special messages:
        MatchDispatchFrom::new(message, &cx)
            .if_request_from(Client, async |_req: InitializeRequest, responder| {
                responder.respond_with_error(crate::Error::invalid_request().data(format!(
                    "proxies must be initialized with `{METHOD_INITIALIZE_PROXY}`"
                )))
            })
            .await
            // Initialize Proxy coming from the client -- forward to the agent but
            // convert into a regular initialize.
            .if_request_from(
                Client,
                async |request: InitializeProxyRequest, responder| {
                    let InitializeProxyRequest { initialize } = request;
                    cx.send_request_to(Agent, initialize)
                        .forward_response_to(responder)
                },
            )
            .await
            // New session coming from the client -- proxy to the agent
            // and add a dynamic handler for that session-id.
            .if_request_from(Client, async |request: NewSessionRequest, responder| {
                let sent = cx.send_request_to(Agent, request);
                // The dynamic-handler hook below means we cannot use
                // `forward_response_to`, so wire up cancellation forwarding
                // explicitly to keep `session/new` cancellable like every
                // other proxied request.
                let sent = sent.forward_cancellation_from(responder.cancellation());
                sent.on_receiving_result({
                    let cx = cx.clone();
                    async move |result| {
                        if let Ok(NewSessionResponse { session_id, .. }) = &result {
                            cx.add_dynamic_handler(ProxySessionMessages::new(session_id.clone()))?
                                .run_indefinitely();
                        }
                        responder.respond_with_result(result)
                    }
                })
            })
            .await
            // Incoming message from the client -- forward to the agent
            .if_message_from(Client, async |message: Dispatch| {
                cx.send_proxied_message_to(Agent, message)
            })
            .await
            // Incoming message from the agent -- forward to the client
            .if_message_from(Agent, async |message: Dispatch| {
                cx.send_proxied_message_to(Client, message)
            })
            .await
            .done()
    }
}

impl Conductor {
    /// Create a connection builder for a conductor.
    pub fn builder(self) -> Builder<Conductor, NullHandler, NullRun> {
        Builder::new(self)
    }
}

impl HasPeer<Client> for Conductor {
    fn remote_style(&self, _peer: Client) -> RemoteStyle {
        RemoteStyle::Predecessor
    }
}

impl HasPeer<Agent> for Conductor {
    fn remote_style(&self, _peer: Agent) -> RemoteStyle {
        RemoteStyle::Successor
    }
}

/// Dynamic handler that proxies session messages from Agent to Client.
///
/// This is used internally to handle session message routing after a
/// `session.new` request has been forwarded.
pub(crate) struct ProxySessionMessages {
    session_id: SessionId,
}

impl ProxySessionMessages {
    /// Create a new proxy handler for the given session.
    pub fn new(session_id: SessionId) -> Self {
        Self { session_id }
    }
}

impl<Counterpart: Role> HandleDispatchFrom<Counterpart> for ProxySessionMessages
where
    Counterpart: HasPeer<Agent> + HasPeer<Client>,
{
    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        connection: ConnectionTo<Counterpart>,
    ) -> Result<Handled<Dispatch>, crate::Error> {
        MatchDispatchFrom::new(message, &connection)
            .if_message_from(Agent, async |message| {
                // If this is for our session-id, proxy it to the client.
                if let Some(session_id) = message.get_session_id()?
                    && session_id == self.session_id
                {
                    connection.send_proxied_message_to(Client, message)?;
                    return Ok(Handled::Yes);
                }

                // Otherwise, leave it alone.
                Ok(Handled::No {
                    message,
                    retry: false,
                })
            })
            .await
            .done()
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        format!("ProxySessionMessages({})", self.session_id)
    }
}
