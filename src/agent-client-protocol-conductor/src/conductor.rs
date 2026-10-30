//! # Conductor: ACP Proxy Chain Orchestrator
//!
//! This module implements the Conductor conductor, which orchestrates a chain of
//! proxy components that sit between an editor and an agent, transforming the
//! Agent-Client Protocol (ACP) stream bidirectionally.
//!
//! ## Architecture Overview
//!
//! The conductor builds and manages a chain of components:
//!
//! ```text
//! Editor <-ACP-> [Component 0] <-ACP-> [Component 1] <-ACP-> ... <-ACP-> Agent
//! ```
//!
//! Each component receives ACP messages, can transform them, and forwards them
//! to the next component in the chain. The conductor:
//!
//! 1. Spawns each component as a subprocess
//! 2. Establishes bidirectional JSON-RPC connections with each component
//! 3. Routes messages between editor, components, and agent
//! 4. Distinguishes proxy vs agent components via distinct request types
//!
//! ## Recursive Chain Building
//!
//! The chain is built recursively through the `proxy/successor` envelope:
//!
//! 1. Editor connects to Component 0 via the conductor
//! 2. When Component 0 wants to communicate with its successor, it sends
//!    a `proxy/successor` request or notification containing the inner method
//!    and params
//! 3. The conductor unwraps the inner message and forwards it to Component 1
//! 4. Component 1 does the same for Component 2, and so on
//! 5. The last component receives the unwrapped ACP message directly
//!
//! This allows each component to be written as if it's talking to a single successor,
//! without knowing about the full chain.
//!
//! ## Proxy vs Agent Initialization
//!
//! Components discover whether they're a proxy or agent via the initialization request they receive:
//!
//! - **Proxy components**: Receive `InitializeProxyRequest` (`proxy/initialize` method)
//! - **Agent component**: Receives standard `InitializeRequest` (`initialize` method)
//!
//! The conductor sends `InitializeProxyRequest` to all proxy components in the chain,
//! and `InitializeRequest` only to the final agent component. This allows proxies to
//! know they should forward messages to a successor, while agents know they are the
//! terminal component
//!
//! ## Message Routing
//!
//! The conductor runs an event loop processing messages from:
//!
//! - **Editor to first component**: Standard ACP messages
//! - **Component to successor**: Via the `proxy/successor` envelope
//! - **Component responses**: Via futures channels back to requesters
//!
//! The message flow ensures bidirectional communication while maintaining the
//! abstraction that each component only knows about its immediate successor.
//!
//! ## Lazy Component Initialization
//!
//! Components are instantiated lazily when the first `initialize` request is received
//! from the editor. This enables dynamic proxy chain construction based on client capabilities.
//!
//! ### Fixed Chains
//!
//! Use [`ProxiesAndAgent`] to assemble a conductor that presents as an agent:
//!
//! ```ignore
//! use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
//!
//! let conductor = ConductorImpl::new_agent(
//!     "my-conductor",
//!     ProxiesAndAgent::new(agent)
//!         .proxy(proxy1)
//!         .proxy(proxy2),
//! );
//! ```
//!
//! A conductor that presents as a proxy takes only its internal proxies; its
//! final successor is supplied when the conductor is connected:
//!
//! ```ignore
//! use agent_client_protocol_conductor::ConductorImpl;
//!
//! let conductor = ConductorImpl::new_proxy("my-proxy-conductor", vec![proxy]);
//! ```
//!
//! ### Dynamic Chain Selection
//!
//! Both constructors also accept an instantiator closure. The closure receives
//! the [`SelectedInitializeRequest`], parsed for the protocol version the
//! conductor selected, and returns the possibly modified request together with
//! type-erased connectors for the selected chain:
//!
//! ```ignore
//! use agent_client_protocol::{Client, Conductor, DynConnectTo};
//! use agent_client_protocol_conductor::{ConductorImpl, SelectedInitializeRequest};
//!
//! let conductor = ConductorImpl::new_agent("my-conductor", |init_req: SelectedInitializeRequest| async move {
//!     // This chain supports only v1; reject any other selected version.
//!     let init_req = init_req.into_v1()?;
//!     let mut proxies: Vec<DynConnectTo<Conductor>> = Vec::new();
//!     if has_auth_capability(&init_req) {
//!         proxies.push(DynConnectTo::new(make_auth_proxy()));
//!     }
//!
//!     let agent: DynConnectTo<Client> = DynConnectTo::new(make_agent());
//!     Ok((init_req.into(), proxies, agent))
//! });
//! ```
//!
//! ## Protocol Versions
//!
//! The conductor selects the chain's protocol version from the first
//! `initialize` request as an agent would, then forwards every version through
//! one untyped path: `proxy/initialize` has the same wire shape in v1 and v2,
//! so the conductor only renames `initialize` to `proxy/initialize` for the
//! first proxy and otherwise forwards messages unchanged. Its connections have
//! no protocol version guard; the endpoints validate the traffic.
//!
//! Proxies negotiate `proxy/initialize` the way agents negotiate `initialize`,
//! so the selected version is an upper bound: a proxy that supports only a
//! lower version initializes the rest of the chain with it, and the
//! `initialize` response the client receives carries the version the whole
//! chain accepted.

use std::sync::Arc;

#[cfg(feature = "unstable_protocol_v2")]
use agent_client_protocol::schema::v2;
use agent_client_protocol::schema::{METHOD_INITIALIZE_PROXY, ProtocolVersion, v1};
use agent_client_protocol::{
    Agent, BoxFuture, Client, Conductor, ConnectTo, Dispatch, DynConnectTo, Error, JsonRpcMessage,
    Proxy, Responder, Role, RunWithConnectionTo, UntypedMessage, role::HasPeer,
    util::MatchDispatch,
};
use agent_client_protocol::{
    Builder, ConnectionTo, JsonRpcNotification, JsonRpcRequest, SentRequest,
};
use agent_client_protocol::{HandleDispatchFrom, util::MatchDispatchFrom};
use agent_client_protocol::{Handled, schema::SuccessorMessage};
use futures::{
    SinkExt, StreamExt,
    channel::mpsc::{self},
};
use tracing::{debug, info};

/// The `initialize` request that starts a conductor's chain, parsed for the
/// protocol version the conductor selected.
///
/// Instantiators ([`InstantiateProxiesAndAgent`] and [`InstantiateProxies`])
/// receive this request and return it, possibly modified, together with the
/// chain's components. The conductor selects the version before
/// instantiation, as an agent does: v2 for a request for v2 or newer (with the
/// `unstable_protocol_v2` feature) and v1 otherwise. A conductor acting as a
/// proxy negotiates its `proxy/initialize` the same way.
///
/// The selected version is the highest the chain may speak. The conductor
/// forwards the request at that version, and each proxy negotiates its
/// `proxy/initialize` like an agent, so a proxy that supports only a lower
/// version initializes its successor with that lower version. The version in
/// the `initialize` response that reaches the client is the one every
/// component accepted; a component that cannot accept it rejects the
/// initialization instead.
///
/// If the instantiator returns the request unchanged and the client asked for
/// exactly the selected version, the conductor forwards the client's original
/// params, so fields this SDK does not know survive. A modified request is
/// forwarded as serialized here.
///
/// New variants are added as the SDK supports more protocol versions.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
// Built once per conductor connection; boxing a variant would only make it
// harder to match on.
#[allow(clippy::large_enum_variant)]
pub enum SelectedInitializeRequest {
    /// The conductor selected ACP protocol v1.
    V1(v1::InitializeRequest),
    /// The conductor selected ACP protocol v2.
    ///
    /// Requires the `unstable_protocol_v2` crate feature.
    #[cfg(feature = "unstable_protocol_v2")]
    V2(v2::InitializeRequest),
}

impl SelectedInitializeRequest {
    /// The protocol version the conductor selected.
    #[must_use]
    pub fn protocol_version(&self) -> ProtocolVersion {
        match self {
            Self::V1(_) => ProtocolVersion::V1,
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2(_) => ProtocolVersion::V2,
        }
    }

    /// The v1 request, for an instantiator that only supports v1.
    ///
    /// # Errors
    ///
    /// Returns an invalid-request error naming the selected version when the
    /// conductor selected another version, so a v1-only instantiator can
    /// reject it with `request.into_v1()?`.
    pub fn into_v1(self) -> Result<v1::InitializeRequest, Error> {
        match self {
            Self::V1(request) => Ok(request),
            #[cfg(feature = "unstable_protocol_v2")]
            other => Err(unsupported_by_instantiator(other.protocol_version())),
        }
    }

    /// Parse `params`, the params of an `initialize` request, for the
    /// `selected` protocol version.
    fn parse(selected: ProtocolVersion, params: &serde_json::Value) -> Result<Self, Error> {
        let params = serde_json::Value::Object(
            agent_client_protocol::util::initialize_params_for_version(params, selected)?,
        );
        #[cfg(feature = "unstable_protocol_v2")]
        if selected == ProtocolVersion::V2 {
            return v2::InitializeRequest::parse_message("initialize", &params).map(Self::V2);
        }
        debug_assert_eq!(selected, ProtocolVersion::V1);
        v1::InitializeRequest::parse_message("initialize", &params).map(Self::V1)
    }

    /// Set the request's `protocolVersion` field to the version of its
    /// variant, which an instantiator cannot change.
    fn reset_protocol_version(&mut self) {
        let version = self.protocol_version();
        match self {
            Self::V1(request) => request.protocol_version = version,
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2(request) => request.protocol_version = version,
        }
    }

    fn to_untyped_message(&self) -> Result<UntypedMessage, Error> {
        match self {
            Self::V1(request) => request.to_untyped_message(),
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2(request) => request.to_untyped_message(),
        }
    }
}

impl From<v1::InitializeRequest> for SelectedInitializeRequest {
    fn from(request: v1::InitializeRequest) -> Self {
        Self::V1(request)
    }
}

#[cfg(feature = "unstable_protocol_v2")]
impl From<v2::InitializeRequest> for SelectedInitializeRequest {
    fn from(request: v2::InitializeRequest) -> Self {
        Self::V2(request)
    }
}

/// The error an instantiator returns for a protocol version it does not
/// support.
#[cfg(feature = "unstable_protocol_v2")]
fn unsupported_by_instantiator(version: ProtocolVersion) -> Error {
    Error::invalid_request().data(format!(
        "this conductor instantiator does not support ACP protocol version {version}"
    ))
}

/// The protocol version requested by an `initialize` or `proxy/initialize`
/// request.
fn requested_protocol_version(request: &UntypedMessage) -> Result<ProtocolVersion, Error> {
    request
        .params()
        .get("protocolVersion")
        .cloned()
        .and_then(|version| serde_json::from_value::<ProtocolVersion>(version).ok())
        .ok_or_else(|| {
            Error::invalid_params()
                .data("initialize.protocolVersion must be a valid ACP protocol version")
        })
}

/// The `initialize` message to forward into the chain after the
/// instantiator returned `modified` for `original`.
///
/// An unchanged request for exactly the selected version is forwarded with the
/// client's original `params`, so unknown fields survive. Anything else is
/// forwarded as serialized from the typed request.
fn forwarded_initialize_request(
    params: serde_json::Value,
    requested: ProtocolVersion,
    original: &SelectedInitializeRequest,
    mut modified: SelectedInitializeRequest,
) -> Result<UntypedMessage, Error> {
    let selected = original.protocol_version();
    if modified.protocol_version() != selected {
        return Err(Error::invalid_request().data(format!(
            "the conductor instantiator must return an initialize request for the selected ACP protocol version {selected}, not {}",
            modified.protocol_version(),
        )));
    }
    modified.reset_protocol_version();
    if modified == *original && requested == selected {
        Ok(UntypedMessage {
            method: "initialize".to_string(),
            params,
        })
    } else {
        modified.to_untyped_message()
    }
}

/// The first request of a conductor connection, accepted as its
/// initialization.
struct AcceptedInitialize {
    /// The client's original params.
    params: serde_json::Value,
    requested: ProtocolVersion,
    request: SelectedInitializeRequest,
    responder: Responder<serde_json::Value>,
}

/// Accept the conductor's first message as its initialization.
///
/// Returns `None` after answering a message that cannot initialize the
/// conductor: the conductor stays uninitialized and the client may retry.
/// Notifications before initialization are dropped.
fn accept_initialize<Host: ConductorHostRole>(
    host: &Host,
    message: Dispatch,
) -> Result<Option<AcceptedInitialize>, Error> {
    let expected = || {
        Error::invalid_request().data(format!("expected `{}` request", host.initialize_method()))
    };
    let (request, responder) = match message {
        Dispatch::Request(request, responder) => (request, responder),
        Dispatch::Notification(notification) => {
            debug!(
                method = notification.method(),
                "dropping notification before conductor initialization"
            );
            return Ok(None);
        }
        Dispatch::Response(_, router) => {
            router.route_with_error(expected())?;
            return Ok(None);
        }
    };
    let accepted = if request.method() == host.initialize_method() {
        requested_protocol_version(&request).and_then(|requested| {
            let selected = host.select_protocol_version(requested)?;
            let parsed = SelectedInitializeRequest::parse(selected, request.params())?;
            Ok((requested, parsed))
        })
    } else {
        Err(expected())
    };
    match accepted {
        Ok((requested, parsed)) => Ok(Some(AcceptedInitialize {
            params: request.params,
            requested,
            request: parsed,
            responder,
        })),
        Err(error) => {
            responder.respond_with_error(error)?;
            Ok(None)
        }
    }
}

/// The conductor manages the proxy chain lifecycle and message routing.
///
/// It maintains connections to all components in the chain and routes messages
/// bidirectionally between the editor, components, and agent.
///
#[derive(Debug)]
pub struct ConductorImpl<Host: ConductorHostRole> {
    host: Host,
    name: String,
    instantiator: Host::Instantiator,
    trace_writer: Option<crate::trace::TraceWriter>,
}

impl<Host: ConductorHostRole> ConductorImpl<Host> {
    pub fn new(host: Host, name: impl ToString, instantiator: Host::Instantiator) -> Self {
        ConductorImpl {
            name: name.to_string(),
            host,
            instantiator,
            trace_writer: None,
        }
    }
}

impl ConductorImpl<Agent> {
    /// Create a conductor in agent mode (the last component is an agent).
    pub fn new_agent(
        name: impl ToString,
        instantiator: impl InstantiateProxiesAndAgent + 'static,
    ) -> Self {
        ConductorImpl::new(Agent, name, Box::new(instantiator))
    }
}

impl ConductorImpl<Proxy> {
    /// Create a conductor in proxy mode (forwards to another conductor).
    pub fn new_proxy(name: impl ToString, instantiator: impl InstantiateProxies + 'static) -> Self {
        ConductorImpl::new(Proxy, name, Box::new(instantiator))
    }
}

impl<Host: ConductorHostRole> ConductorImpl<Host> {
    /// Enable trace logging to a custom destination.
    ///
    /// Use `agent-client-protocol-trace-viewer` to view the trace as an interactive sequence diagram.
    #[must_use]
    pub fn trace_to(mut self, dest: impl crate::trace::WriteEvent) -> Self {
        self.trace_writer = Some(crate::trace::TraceWriter::new(dest));
        self
    }

    /// Enable trace logging to a file path.
    ///
    /// Events will be written as newline-delimited JSON (`.jsons` format).
    /// Use `agent-client-protocol-trace-viewer` to view the trace as an interactive sequence diagram.
    pub fn trace_to_path(mut self, path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        self.trace_writer = Some(crate::trace::TraceWriter::from_path(path)?);
        Ok(self)
    }

    /// Enable trace logging with an existing TraceWriter.
    #[must_use]
    pub fn with_trace_writer(mut self, writer: crate::trace::TraceWriter) -> Self {
        self.trace_writer = Some(writer);
        self
    }

    /// Run the conductor with a transport.
    pub async fn run(
        self,
        transport: impl ConnectTo<Host>,
    ) -> Result<(), agent_client_protocol::Error> {
        let (conductor_tx, conductor_rx) = mpsc::channel(128 /* chosen arbitrarily */);

        // Set up tracing if enabled - spawn writer task and get handle
        let trace_handle;
        let trace_future: BoxFuture<'static, Result<(), agent_client_protocol::Error>>;
        if let Some((h, f)) = self.trace_writer.map(super::trace::TraceWriter::spawn) {
            trace_handle = Some(h);
            trace_future = Box::pin(f);
        } else {
            trace_handle = None;
            trace_future = Box::pin(std::future::ready(Ok(())));
        }

        let runner = ConductorRunner {
            conductor_rx,
            conductor_tx: conductor_tx.clone(),
            initialization: InitializationState::Pending(self.instantiator),
            proxies: Vec::default(),
            successor: Arc::new(agent_client_protocol::util::internal_error(
                "successor not initialized",
            )),
            trace_handle,
            host: self.host.clone(),
        };

        // The conductor forwards every protocol version untyped, so its
        // connection to the client has no version guard. The conductor
        // selects the version during initialization; the endpoints validate
        // the traffic of that version.
        Builder::new_with(
            self.host.clone(),
            ConductorMessageHandler {
                conductor_tx,
                host: self.host.clone(),
            },
        )
        .without_acp_version_guard()
        .name(self.name)
        .with_runner(runner)
        .with_spawned(|_cx| trace_future)
        .connect_to(transport)
        .await
    }

    async fn incoming_message_from_client(
        conductor_tx: &mut mpsc::Sender<ConductorMessage>,
        message: Dispatch,
    ) -> Result<(), agent_client_protocol::Error> {
        conductor_tx
            .send(ConductorMessage::LeftToRight {
                target_component_index: 0,
                message,
            })
            .await
            .map_err(agent_client_protocol::util::internal_error)
    }

    async fn incoming_message_from_agent(
        conductor_tx: &mut mpsc::Sender<ConductorMessage>,
        message: Dispatch,
    ) -> Result<(), agent_client_protocol::Error> {
        conductor_tx
            .send(ConductorMessage::RightToLeft {
                source_component_index: SourceComponentIndex::Successor,
                message,
            })
            .await
            .map_err(agent_client_protocol::util::internal_error)
    }
}

impl<Host: ConductorHostRole> ConnectTo<Host::Counterpart> for ConductorImpl<Host> {
    async fn connect_to(
        self,
        client: impl ConnectTo<Host>,
    ) -> Result<(), agent_client_protocol::Error> {
        self.run(client).await
    }
}

struct ConductorMessageHandler<Host: ConductorHostRole> {
    conductor_tx: mpsc::Sender<ConductorMessage>,
    host: Host,
}

impl<Host: ConductorHostRole> HandleDispatchFrom<Host::Counterpart>
    for ConductorMessageHandler<Host>
{
    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        connection: agent_client_protocol::ConnectionTo<Host::Counterpart>,
    ) -> Result<agent_client_protocol::Handled<Dispatch>, agent_client_protocol::Error> {
        self.host
            .handle_dispatch(message, connection, &mut self.conductor_tx)
            .await
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        "ConductorMessageHandler"
    }
}

/// The conductor manages the proxy chain lifecycle and message routing.
///
/// It maintains connections to all components in the chain and routes messages
/// bidirectionally between the editor, components, and agent.
///
pub struct ConductorRunner<Host>
where
    Host: ConductorHostRole,
{
    conductor_rx: mpsc::Receiver<ConductorMessage>,

    conductor_tx: mpsc::Sender<ConductorMessage>,

    /// The initialization lifecycle. Holds the instantiator until the first
    /// `initialize` request is accepted.
    initialization: InitializationState<Host::Instantiator>,

    /// The chain of proxies before the agent (if any).
    ///
    /// Populated lazily when the first Initialize request is received.
    proxies: Vec<ConnectionTo<Proxy>>,

    /// If the conductor is operating in agent mode, this will direct messages to the agent.
    /// If the conductor is operating in proxy mode, this will direct messages to the successor.
    /// Populated lazily when the first Initialize request is received; the initial value just returns errors.
    successor: Arc<dyn ConductorSuccessor<Host>>,

    /// Optional trace handle for sequence diagram visualization.
    trace_handle: Option<crate::trace::TraceHandle>,

    /// Defines what sort of link we have
    host: Host,
}

enum InitializationState<Instantiator> {
    Pending(Instantiator),
    Initializing,
    Ready,
    Failed(Error),
}

impl<Host> std::fmt::Debug for ConductorRunner<Host>
where
    Host: ConductorHostRole,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConductorRunner")
            .field("conductor_rx", &self.conductor_rx)
            .field("conductor_tx", &self.conductor_tx)
            .field("proxies", &self.proxies)
            .field("trace_handle", &self.trace_handle)
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

impl<Host> RunWithConnectionTo<Host::Counterpart> for ConductorRunner<Host>
where
    Host: ConductorHostRole,
{
    async fn run_with_connection_to(
        mut self,
        connection: ConnectionTo<Host::Counterpart>,
    ) -> Result<(), agent_client_protocol::Error> {
        // Components are now spawned lazily in forward_initialize_request
        // when the first Initialize request is received.

        // This is the "central actor" of the conductor. Most other things forward messages
        // via `conductor_tx` into this loop. This lets us serialize the conductor's activity.
        while let Some(message) = self.conductor_rx.next().await {
            self.handle_conductor_message(connection.clone(), message)
                .await?;
        }
        Ok(())
    }
}

impl<Host> ConductorRunner<Host>
where
    Host: ConductorHostRole,
{
    /// Recursively spawns components and builds the proxy chain.
    ///
    /// This function implements the recursive chain building pattern:
    /// 1. Pop the next component from the `providers` list
    /// 2. Create the component (either spawn subprocess or use mock)
    /// 3. Set up JSON-RPC connection and message handlers
    /// 4. Recursively call itself to spawn the next component
    /// 5. When no components remain, continue in the central runner's message-routing loop
    ///
    /// Central message handling logic for the conductor.
    /// The conductor routes all [`ConductorMessage`] messages through to this function.
    /// Each message corresponds to a request or notification from one component to another.
    /// The conductor ferries messages from one place to another, sometimes making modifications along the way.
    /// Note that *responses to requests* are sent *directly* without going through this loop.
    ///
    /// The names we use are
    ///
    /// * The *client* is the originator of all ACP traffic, typically an editor or GUI.
    /// * Then there is a sequence of *components* consisting of:
    ///     * Zero or more *proxies*, which receive messages and forward them to the next component in the chain.
    ///     * And finally the *agent*, which is the final component in the chain and handles the actual work.
    ///
    /// For the most part, we pass messages through the chain without modification. The initialization
    /// handshake is the exception:
    ///
    /// * We send `InitializeProxyRequest` to proxy components and `InitializeRequest` to the agent component.
    async fn handle_conductor_message(
        &mut self,
        client: ConnectionTo<Host::Counterpart>,
        message: ConductorMessage,
    ) -> Result<(), agent_client_protocol::Error> {
        let (direction, dispatch) = match &message {
            ConductorMessage::LeftToRight { message, .. } => ("left_to_right", message),
            ConductorMessage::RightToLeft { message, .. } => ("right_to_left", message),
        };
        tracing::debug!(
            direction,
            method = dispatch.method(),
            id = ?dispatch.raw_id(),
            "handle_conductor_message"
        );

        match message {
            ConductorMessage::LeftToRight {
                target_component_index,
                message,
            } => {
                // Tracing happens inside forward_client_to_agent_message, after initialization,
                // so that component_name() has access to the populated proxies list.
                self.forward_client_to_agent_message(target_component_index, message, client)
                    .await
            }

            ConductorMessage::RightToLeft {
                source_component_index,
                message,
            } => {
                tracing::debug!(
                    ?source_component_index,
                    message_method = ?message.method(),
                    "Conductor: AgentToClient received"
                );
                self.send_message_to_predecessor_of(client, source_component_index, message)
            }
        }
    }

    /// Send a message (request or notification) to the predecessor of the given component.
    ///
    /// This is a bit subtle because the relationship of the conductor
    /// is different depending on who will be receiving the message:
    /// * If the message is going to the conductor's client, then no changes
    ///   are needed, as the conductor is sending an agent-to-client message and
    ///   the conductor is acting as the agent.
    /// * If the message is going to a proxy component, then we have to wrap
    ///   it in a "from successor" wrapper, because the conductor is the
    ///   proxy's client.
    fn send_message_to_predecessor_of<Req: JsonRpcRequest, N: JsonRpcNotification>(
        &mut self,
        client: ConnectionTo<Host::Counterpart>,
        source_component_index: SourceComponentIndex,
        message: Dispatch<Req, N>,
    ) -> Result<(), agent_client_protocol::Error>
    where
        Req::Response: Send,
    {
        let source_component_index = match source_component_index {
            SourceComponentIndex::Successor => self.proxies.len(),
            SourceComponentIndex::Proxy(index) => index,
        };

        match message {
            Dispatch::Request(request, responder) => self
                .send_request_to_predecessor_of(client, source_component_index, request)
                .forward_response_to(responder),
            Dispatch::Notification(notification) => {
                // `$/cancel_request` is connection-scoped: its `requestId` was
                // allocated on the connection the notification arrived over
                // and means nothing on the predecessor's connection. The SDK
                // already propagates the cancellation hop by hop through the
                // `forward_response_to` calls above, so drop the raw
                // notification instead of tunneling a meaningless ID.
                if agent_client_protocol::is_cancel_request_notification(&notification) {
                    tracing::debug!(
                        "not forwarding hop-scoped `$/cancel_request` notification to predecessor"
                    );
                    return Ok(());
                }
                self.send_notification_to_predecessor_of(
                    client,
                    source_component_index,
                    notification,
                )
            }
            Dispatch::Response(result, router) => router.route_with_result(result),
        }
    }

    fn send_request_to_predecessor_of<Req: JsonRpcRequest>(
        &mut self,
        client_connection: ConnectionTo<Host::Counterpart>,
        source_component_index: usize,
        request: Req,
    ) -> SentRequest<Req::Response> {
        if source_component_index == 0 {
            client_connection.send_request_to(Client, request)
        } else {
            self.proxies[source_component_index - 1].send_request(SuccessorMessage {
                message: request,
                meta: None,
            })
        }
    }

    /// Send a notification to the predecessor of the given component.
    ///
    /// This is a bit subtle because the relationship of the conductor
    /// is different depending on who will be receiving the message:
    /// * If the notification is going to the conductor's client, then no changes
    ///   are needed, as the conductor is sending an agent-to-client message and
    ///   the conductor is acting as the agent.
    /// * If the notification is going to a proxy component, then we have to wrap
    ///   it in a "from successor" wrapper, because the conductor is the
    ///   proxy's client.
    fn send_notification_to_predecessor_of<N: JsonRpcNotification>(
        &mut self,
        client: ConnectionTo<Host::Counterpart>,
        source_component_index: usize,
        notification: N,
    ) -> Result<(), agent_client_protocol::Error> {
        tracing::debug!(
            source_component_index,
            proxies_len = self.proxies.len(),
            "send_notification_to_predecessor_of"
        );
        if source_component_index == 0 {
            tracing::debug!("Sending notification directly to client");
            client.send_notification_to(Client, notification)
        } else {
            tracing::debug!(
                target_proxy = source_component_index - 1,
                "Sending notification wrapped as SuccessorMessage to proxy"
            );
            self.proxies[source_component_index - 1].send_notification(SuccessorMessage {
                message: notification,
                meta: None,
            })
        }
    }

    /// Send a message (request or notification) from 'left to right'.
    /// Left-to-right means from the client or an intermediate proxy to the component
    /// at `target_component_index` (could be a proxy or the agent).
    /// Ensures the component chain is initialized before forwarding the message.
    async fn forward_client_to_agent_message(
        &mut self,
        target_component_index: usize,
        message: Dispatch,
        client: ConnectionTo<Host::Counterpart>,
    ) -> Result<(), agent_client_protocol::Error> {
        tracing::trace!(
            target_component_index,
            method = message.method(),
            id = ?message.raw_id(),
            "forward_client_to_agent_message"
        );

        // Ensure components are initialized before processing any message.
        let Some(message) = self.ensure_initialized(client.clone(), message).await? else {
            return Ok(());
        };

        // In proxy mode, if the target is beyond our component chain,
        // forward to the conductor's own successor (via client connection)
        if target_component_index < self.proxies.len() {
            self.forward_message_from_client_to_proxy(target_component_index, message)
        } else {
            assert_eq!(target_component_index, self.proxies.len());

            debug!(
                target_component_index,
                proxies_count = self.proxies.len(),
                "Proxy mode: forwarding successor message to conductor's successor"
            );
            let successor = self.successor.clone();
            successor.send_message(message, client, self).await
        }
    }

    /// Ensures components are initialized before processing messages.
    ///
    /// If components haven't been initialized yet, this expects the first message
    /// to be an `initialize` request (`proxy/initialize` for a conductor acting
    /// as a proxy), selects the chain's protocol version, and uses the request to
    /// instantiate and spawn the component chain.
    ///
    /// A first message that cannot initialize the conductor is answered with an
    /// error and leaves the conductor uninitialized, so the client may retry. If
    /// instantiation fails, the error answers the `initialize` request and every
    /// later message.
    ///
    /// Returns:
    /// - `Ok(Some(message))` - Components are initialized, continue processing this message
    /// - `Ok(None)` - The message was answered or dropped, caller should return early
    /// - `Err(_)` - A fatal error occurred
    async fn ensure_initialized(
        &mut self,
        client: ConnectionTo<Host::Counterpart>,
        message: Dispatch,
    ) -> Result<Option<Dispatch>, Error> {
        let state = std::mem::replace(&mut self.initialization, InitializationState::Initializing);
        match state {
            InitializationState::Pending(instantiator) => {
                let host = self.host.clone();
                let Some(accepted) = accept_initialize(&host, message)? else {
                    self.initialization = InitializationState::Pending(instantiator);
                    return Ok(None);
                };
                let AcceptedInitialize {
                    params,
                    requested,
                    request,
                    responder,
                } = accepted;
                debug!(
                    %requested,
                    selected = %request.protocol_version(),
                    "ensure_initialized: instantiating chain"
                );
                let original = request.clone();
                let forwarded = match host.instantiate(request, client, instantiator, self).await {
                    Ok(modified) => {
                        forwarded_initialize_request(params, requested, &original, modified)
                    }
                    Err(error) => Err(error),
                };
                match forwarded {
                    Ok(message) => {
                        self.initialization = InitializationState::Ready;
                        Ok(Some(Dispatch::Request(message, responder)))
                    }
                    Err(error) => {
                        self.initialization = InitializationState::Failed(error.clone());
                        responder.respond_with_error(error)?;
                        Ok(None)
                    }
                }
            }
            InitializationState::Ready => {
                self.initialization = InitializationState::Ready;
                Ok(Some(message))
            }
            InitializationState::Failed(error) => {
                let result = match message {
                    Dispatch::Request(_, responder) => responder.respond_with_error(error.clone()),
                    Dispatch::Notification(_) => Ok(()),
                    Dispatch::Response(_, router) => router.route_with_error(error.clone()),
                };
                self.initialization = InitializationState::Failed(error);
                result?;
                Ok(None)
            }
            InitializationState::Initializing => {
                Err(Error::internal_error().data("conductor initialization was re-entered"))
            }
        }
    }

    /// Wrap a proxy component with tracing if tracing is enabled.
    ///
    /// Returns the component unchanged if tracing is disabled.
    fn trace_proxy(
        &self,
        proxy_index: ComponentIndex,
        successor_index: ComponentIndex,
        component: impl ConnectTo<Conductor>,
    ) -> DynConnectTo<Conductor> {
        match &self.trace_handle {
            Some(trace_handle) => {
                trace_handle.bridge_component(proxy_index, successor_index, component)
            }
            None => DynConnectTo::new(component),
        }
    }

    /// Spawn proxy components and add them to the proxies list.
    fn spawn_proxies(
        &mut self,
        client: ConnectionTo<Host::Counterpart>,
        proxy_components: Vec<DynConnectTo<Conductor>>,
    ) -> Result<(), agent_client_protocol::Error> {
        assert!(self.proxies.is_empty());

        let num_proxies = proxy_components.len();
        info!(proxy_count = num_proxies, "spawn_proxies");

        // Special case: if there are no user-defined proxies
        // but tracing is enabled, we make a dummy proxy that just
        // passes through messages but which can trigger the
        // tracing events.
        if self.trace_handle.is_some() && num_proxies == 0 {
            // The pass-through proxy forwards untyped messages, so one
            // guard-less proxy serves every protocol version. The endpoints
            // validate the selected version themselves.
            let trace_proxy = DynConnectTo::new(Builder::new(Proxy).without_acp_version_guard());

            self.connect_to_proxy(
                &client,
                0,
                ComponentIndex::Client,
                ComponentIndex::Agent,
                trace_proxy,
            )?;
        } else {
            // Spawn each proxy component
            for (component_index, dyn_component) in proxy_components.into_iter().enumerate() {
                debug!(component_index, "spawning proxy");

                self.connect_to_proxy(
                    &client,
                    component_index,
                    ComponentIndex::Proxy(component_index),
                    ComponentIndex::successor_of(component_index, num_proxies),
                    dyn_component,
                )?;
            }
        }

        info!(proxy_count = self.proxies.len(), "Proxies spawned");

        Ok(())
    }

    /// Create a connection to the proxy with index `component_index` implemented in `component`.
    ///
    /// If tracing is enabled, the proxy's index is `trace_proxy_index` and its successor is `trace_successor_index`.
    fn connect_to_proxy(
        &mut self,
        client: &ConnectionTo<Host::Counterpart>,
        component_index: usize,
        trace_proxy_index: ComponentIndex,
        trace_successor_index: ComponentIndex,
        component: impl ConnectTo<Conductor>,
    ) -> Result<(), Error> {
        let connection_builder = self.connection_to_proxy(component_index);
        let connect_component =
            self.trace_proxy(trace_proxy_index, trace_successor_index, component);
        let proxy_connection = client.spawn_connection(connection_builder, connect_component)?;
        self.proxies.push(proxy_connection);
        Ok(())
    }

    /// Create the conductor's connection to the proxy with index `component_index`.
    ///
    /// Outgoing messages received from the proxy are sent to `self.conductor_tx` as either
    /// left-to-right or right-to-left messages depending on whether they are wrapped
    /// in `SuccessorMessage`.
    fn connection_to_proxy(
        &mut self,
        component_index: usize,
    ) -> Builder<Conductor, impl HandleDispatchFrom<Proxy> + 'static> {
        type SuccessorDispatch = Dispatch<SuccessorMessage, SuccessorMessage>;
        let mut conductor_tx = self.conductor_tx.clone();
        Conductor
            .builder()
            .name(format!("conductor-to-component({component_index})"))
            // Intercept messages sent by the proxy.
            .on_receive_dispatch(
                async move |dispatch: Dispatch, _connection| {
                    MatchDispatch::new(dispatch)
                        .if_dispatch(async |dispatch: SuccessorDispatch| {
                            //                         ------------------
                            // SuccessorMessages sent by the proxy go to its successor.
                            //
                            // Subtle point:
                            //
                            // `ConductorToProxy` has only a single peer, `Agent`. This means that we see
                            // "successor messages" in their "desugared form". So when we intercept an *outgoing*
                            // message that matches `SuccessorMessage`, it could be one of three things
                            //
                            // - A request being sent by the proxy to its successor (hence going left->right)
                            // - A notification being sent by the proxy to its successor (hence going left->right)
                            // - A response to a request sent to the proxy *by* its successor. Here, the *request*
                            //   was going right->left, but the *response* (the message we are processing now)
                            //   is going left->right.
                            //
                            // So, in all cases, we forward as a left->right message.

                            conductor_tx
                                .send(ConductorMessage::LeftToRight {
                                    target_component_index: component_index + 1,
                                    message: dispatch.map(|r, cx| (r.message, cx), |n| n.message),
                                })
                                .await
                                .map_err(agent_client_protocol::util::internal_error)
                        })
                        .await
                        .otherwise(async |dispatch| {
                            // Other messagrs send by the proxy go its predecessor.
                            // As in the previous handler:
                            //
                            // Messages here are seen in their "desugared form", so we are seeing
                            // one of three things
                            //
                            // - A request being sent by the proxy to its predecessor (hence going right->left)
                            // - A notification being sent by the proxy to its predecessor (hence going right->left)
                            // - A response to a request sent to the proxy *by* its predecessor. Here, the *request*
                            //   was going left->right, but the *response* (the message we are processing now)
                            //   is going right->left.
                            //
                            // So, in all cases, we forward as a right->left message.

                            let message = ConductorMessage::RightToLeft {
                                source_component_index: SourceComponentIndex::Proxy(
                                    component_index,
                                ),
                                message: dispatch,
                            };
                            conductor_tx
                                .send(message)
                                .await
                                .map_err(agent_client_protocol::util::internal_error)
                        })
                        .await
                },
                agent_client_protocol::on_receive_dispatch!(),
            )
    }

    /// Forward a message from the client (or the previous proxy) to the proxy
    /// with index `target_component_index`.
    ///
    /// Forwarding is untyped and the same for every protocol version:
    /// `proxy/initialize` has the same wire shape in v1 and v2, so the
    /// conductor never parses or re-serializes forwarded messages, and fields
    /// it does not know survive.
    fn forward_message_from_client_to_proxy(
        &mut self,
        target_component_index: usize,
        message: Dispatch,
    ) -> Result<(), agent_client_protocol::Error> {
        tracing::debug!(
            method = message.method(),
            id = ?message.raw_id(),
            "forward_message_to_proxy"
        );

        let proxy = &self.proxies[target_component_index];
        match message {
            Dispatch::Request(request, responder)
                if request.method() == METHOD_INITIALIZE_PROXY =>
            {
                responder.respond_with_error(initialize_proxy_from_outside())
            }
            Dispatch::Request(mut request, responder)
                if v1::InitializeRequest::matches_method(request.method()) =>
            {
                // Proxies receive incoming `initialize` messages as if they
                // were a client. The conductor (us) renames them to
                // `proxy/initialize`, keeping the params untouched. The proxy
                // then initializes itself and forwards an `initialize` request
                // to its successor.
                request.method = METHOD_INITIALIZE_PROXY.to_string();
                // The request is rewritten, so `forward_response_to` cannot be
                // used here; wire up cancellation forwarding explicitly to
                // keep `initialize` cancellable like every other forwarded
                // request.
                proxy
                    .send_request(request)
                    .forward_cancellation_from(responder.cancellation())
                    .on_receiving_result(async move |result| {
                        tracing::debug!(
                            ok = result.is_ok(),
                            "got initialize_proxy response from proxy"
                        );
                        responder.respond_with_result(result)
                    })
            }
            message => proxy.send_proxied_message(message),
        }
    }
}

/// Invoked when sending a message from the conductor to the agent that it manages.
/// This is called by the runner's successor's [`ConductorSuccessor::send_message`]
/// method when `Link = ConductorToClient` (i.e., the conductor is not itself
/// running as a proxy).
///
/// Like `ConductorRunner::forward_message_from_client_to_proxy`, this forwards
/// untyped messages unchanged for every protocol version.
fn forward_message_to_agent(
    message: Dispatch,
    agent_connection: &ConnectionTo<Agent>,
) -> Result<(), Error> {
    match message {
        Dispatch::Request(request, responder) if request.method() == METHOD_INITIALIZE_PROXY => {
            responder.respond_with_error(initialize_proxy_from_outside())
        }
        message => agent_connection.send_proxied_message_to(Agent, message),
    }
}

/// The error for a `proxy/initialize` request that arrives from outside the
/// conductor: only the conductor initializes its proxies.
fn initialize_proxy_from_outside() -> Error {
    Error::invalid_request().data("initialize/proxy requests are only sent by the conductor")
}

/// Identifies a component in the conductor's chain for tracing purposes.
///
/// Used to track message sources and destinations through the proxy chain.
#[derive(Debug, Clone, Copy)]
pub enum ComponentIndex {
    /// The client (editor) at the start of the chain.
    Client,

    /// A proxy component at the given index.
    Proxy(usize),

    /// The successor (agent in agent mode, outer conductor in proxy mode).
    Agent,
}

impl ComponentIndex {
    /// Return the index for the predecessor of `proxy_index`, which might be `Client`.
    #[must_use]
    pub fn predecessor_of(proxy_index: usize) -> Self {
        match proxy_index.checked_sub(1) {
            Some(p_i) => ComponentIndex::Proxy(p_i),
            None => ComponentIndex::Client,
        }
    }

    /// Return the index for the predecessor of `proxy_index`, which might be `Client`.
    #[must_use]
    pub fn successor_of(proxy_index: usize, num_proxies: usize) -> Self {
        if proxy_index == num_proxies {
            ComponentIndex::Agent
        } else {
            ComponentIndex::Proxy(proxy_index + 1)
        }
    }
}

/// Identifies the source of an agent-to-client message.
///
/// This enum handles the fact that the conductor may receive messages from two different sources:
/// 1. From one of its managed components (identified by index)
/// 2. From the conductor's own successor in a larger proxy chain (when in proxy mode)
#[derive(Debug, Clone, Copy)]
pub enum SourceComponentIndex {
    /// Message from a specific component at the given index in the managed chain.
    Proxy(usize),

    /// Message from the conductor's agent or successor.
    Successor,
}

/// Trait for lazy proxy instantiation (proxy mode).
///
/// Used by conductors in proxy mode (`ConductorToConductor`) where all components
/// are proxies that forward to an outer conductor.
pub trait InstantiateProxies: Send {
    /// Instantiate proxy components based on the `initialize` request.
    ///
    /// The request is parsed for the protocol version the conductor selected;
    /// see [`SelectedInitializeRequest`]. An implementation that supports only
    /// some versions rejects the others with an error, for example with
    /// [`SelectedInitializeRequest::into_v1`].
    ///
    /// Returns the (possibly modified) request and the proxy components, typed
    /// as `DynConnectTo<Conductor>` since proxies communicate with the
    /// conductor.
    fn instantiate_proxies(
        self: Box<Self>,
        req: SelectedInitializeRequest,
    ) -> futures::future::BoxFuture<
        'static,
        Result<
            (SelectedInitializeRequest, Vec<DynConnectTo<Conductor>>),
            agent_client_protocol::Error,
        >,
    >;
}

/// Simple implementation: provide all proxy components unconditionally, for
/// every protocol version.
///
/// Requires `T: ConnectTo<Conductor>`.
impl<T> InstantiateProxies for Vec<T>
where
    T: ConnectTo<Conductor> + 'static,
{
    fn instantiate_proxies(
        self: Box<Self>,
        req: SelectedInitializeRequest,
    ) -> futures::future::BoxFuture<
        'static,
        Result<
            (SelectedInitializeRequest, Vec<DynConnectTo<Conductor>>),
            agent_client_protocol::Error,
        >,
    > {
        Box::pin(async move {
            let components: Vec<DynConnectTo<Conductor>> =
                (*self).into_iter().map(|c| DynConnectTo::new(c)).collect();
            Ok((req, components))
        })
    }
}

/// Dynamic implementation: closure receives the `initialize` request and returns proxies.
impl<F, Fut> InstantiateProxies for F
where
    F: FnOnce(SelectedInitializeRequest) -> Fut + Send + 'static,
    Fut: std::future::Future<
            Output = Result<
                (SelectedInitializeRequest, Vec<DynConnectTo<Conductor>>),
                agent_client_protocol::Error,
            >,
        > + Send
        + 'static,
{
    fn instantiate_proxies(
        self: Box<Self>,
        req: SelectedInitializeRequest,
    ) -> futures::future::BoxFuture<
        'static,
        Result<
            (SelectedInitializeRequest, Vec<DynConnectTo<Conductor>>),
            agent_client_protocol::Error,
        >,
    > {
        Box::pin(async move { (*self)(req).await })
    }
}

/// The result of [`InstantiateProxiesAndAgent::instantiate_proxies_and_agent`]:
/// the (possibly modified) request, the proxy components, and the agent
/// component.
pub type ProxiesAndAgentComponents = (
    SelectedInitializeRequest,
    Vec<DynConnectTo<Conductor>>,
    DynConnectTo<Client>,
);

/// Trait for lazy proxy and agent instantiation (agent mode).
///
/// Used by conductors in agent mode (`ConductorToClient`) where there are
/// zero or more proxies followed by an agent component.
pub trait InstantiateProxiesAndAgent: Send {
    /// Instantiate proxy and agent components based on the `initialize` request.
    ///
    /// The request is parsed for the protocol version the conductor selected;
    /// see [`SelectedInitializeRequest`]. An implementation that supports only
    /// some versions rejects the others with an error, for example with
    /// [`SelectedInitializeRequest::into_v1`].
    ///
    /// Returns the (possibly modified) request, a vector of proxy components
    /// (typed as `DynConnectTo<Conductor>`), and the agent component
    /// (typed as `DynConnectTo<Client>`).
    fn instantiate_proxies_and_agent(
        self: Box<Self>,
        req: SelectedInitializeRequest,
    ) -> futures::future::BoxFuture<
        'static,
        Result<ProxiesAndAgentComponents, agent_client_protocol::Error>,
    >;
}

/// Wrapper to convert a single agent component (no proxies) into
/// `InstantiateProxiesAndAgent`, for every protocol version.
#[derive(Debug)]
pub struct AgentOnly<A>(pub A);

impl<A: ConnectTo<Client> + 'static> InstantiateProxiesAndAgent for AgentOnly<A> {
    fn instantiate_proxies_and_agent(
        self: Box<Self>,
        req: SelectedInitializeRequest,
    ) -> futures::future::BoxFuture<
        'static,
        Result<ProxiesAndAgentComponents, agent_client_protocol::Error>,
    > {
        Box::pin(async move { Ok((req, Vec::new(), DynConnectTo::new(self.0))) })
    }
}

/// Builder for creating proxies and agent components.
///
/// The components are used for every protocol version.
///
/// # Example
/// ```ignore
/// ProxiesAndAgent::new(ElizaAgent::new())
///     .proxy(LoggingProxy::new())
///     .proxy(AuthProxy::new())
/// ```
#[derive(Debug)]
pub struct ProxiesAndAgent {
    proxies: Vec<DynConnectTo<Conductor>>,
    agent: DynConnectTo<Client>,
}

impl ProxiesAndAgent {
    /// Create a new builder with the given agent component.
    pub fn new(agent: impl ConnectTo<Client> + 'static) -> Self {
        Self {
            proxies: vec![],
            agent: DynConnectTo::new(agent),
        }
    }

    /// Add a single proxy component.
    #[must_use]
    pub fn proxy(mut self, proxy: impl ConnectTo<Conductor> + 'static) -> Self {
        self.proxies.push(DynConnectTo::new(proxy));
        self
    }

    /// Add multiple proxy components.
    #[must_use]
    pub fn proxies<P, I>(mut self, proxies: I) -> Self
    where
        P: ConnectTo<Conductor> + 'static,
        I: IntoIterator<Item = P>,
    {
        self.proxies
            .extend(proxies.into_iter().map(DynConnectTo::new));
        self
    }
}

impl InstantiateProxiesAndAgent for ProxiesAndAgent {
    fn instantiate_proxies_and_agent(
        self: Box<Self>,
        req: SelectedInitializeRequest,
    ) -> futures::future::BoxFuture<
        'static,
        Result<ProxiesAndAgentComponents, agent_client_protocol::Error>,
    > {
        Box::pin(async move { Ok((req, self.proxies, self.agent)) })
    }
}

/// Dynamic implementation: closure receives the `initialize` request and returns proxies + agent.
impl<F, Fut> InstantiateProxiesAndAgent for F
where
    F: FnOnce(SelectedInitializeRequest) -> Fut + Send + 'static,
    Fut: std::future::Future<
            Output = Result<ProxiesAndAgentComponents, agent_client_protocol::Error>,
        > + Send
        + 'static,
{
    fn instantiate_proxies_and_agent(
        self: Box<Self>,
        req: SelectedInitializeRequest,
    ) -> futures::future::BoxFuture<
        'static,
        Result<ProxiesAndAgentComponents, agent_client_protocol::Error>,
    > {
        Box::pin(async move { (*self)(req).await })
    }
}

/// Messages sent to the conductor's main event loop for routing.
///
/// These messages enable the conductor to route communication between:
/// - The editor and the first component
/// - Components and their successors in the chain
/// - Components and their clients (editor or predecessor)
///
/// All spawned tasks send messages via this enum through a shared channel,
/// allowing centralized routing logic in the conductor runner's event loop.
#[derive(Debug)]
pub enum ConductorMessage {
    /// If this message is a request or notification, then it is going "left-to-right"
    /// (e.g., a component making a request of its successor).
    ///
    /// If this message is a response, then it is going right-to-left
    /// (i.e., the successor answering a request made by its predecessor).
    LeftToRight {
        target_component_index: usize,
        message: Dispatch,
    },

    /// If this message is a request or notification, then it is going "right-to-left"
    /// (e.g., a component making a request of its predecessor).
    ///
    /// If this message is a response, then it is going "left-to-right"
    /// (i.e., the predecessor answering a request made by its successor).
    RightToLeft {
        source_component_index: SourceComponentIndex,
        message: Dispatch,
    },
}

/// Trait implemented for the two links the conductor can use:
///
/// * ConductorToClient -- conductor is acting as an agent, so when its last proxy sends to its successor, the conductor sends that message to its agent component
/// * ConductorToConductor -- conductor is acting as a proxy, so when its last proxy sends to its successor, the (inner) conductor sends that message to its successor, via the outer conductor
///
/// This trait is sealed: it is implemented for [`Agent`] and [`Proxy`] and
/// cannot be implemented outside this crate.
pub trait ConductorHostRole: sealed::Sealed + Role<Counterpart: HasPeer<Client>> {
    /// The type used to instantiate components for this link type.
    type Instantiator: Send;

    /// The method of the request that initializes the conductor:
    /// `initialize`, or `proxy/initialize` for a conductor acting as a proxy.
    fn initialize_method(&self) -> &'static str;

    /// Select the chain's protocol version for an initialization that
    /// requested `requested`.
    ///
    /// # Errors
    ///
    /// Returns the error that answers the initialization when the conductor
    /// supports no suitable version.
    fn select_protocol_version(&self, requested: ProtocolVersion)
    -> Result<ProtocolVersion, Error>;

    /// Instantiate and spawn the chain's components for `request`.
    ///
    /// Takes ownership of the instantiator and returns the (possibly
    /// modified) request to forward into the chain. An error answers the
    /// initialization and every later message.
    fn instantiate(
        &self,
        request: SelectedInitializeRequest,
        connection: ConnectionTo<Self::Counterpart>,
        instantiator: Self::Instantiator,
        runner: &mut ConductorRunner<Self>,
    ) -> impl Future<Output = Result<SelectedInitializeRequest, agent_client_protocol::Error>> + Send;

    /// Handle an incoming message from the client or conductor, depending on `Self`
    fn handle_dispatch(
        &self,
        message: Dispatch,
        connection: ConnectionTo<Self::Counterpart>,
        conductor_tx: &mut mpsc::Sender<ConductorMessage>,
    ) -> impl Future<Output = Result<Handled<Dispatch>, agent_client_protocol::Error>> + Send;
}

/// Select the highest version this build supports at or below `requested`,
/// as an agent does, keeping v1's fallback-to-latest rule for a request below
/// v1.
fn select_like_an_agent(requested: ProtocolVersion) -> ProtocolVersion {
    #[cfg(feature = "unstable_protocol_v2")]
    if requested >= ProtocolVersion::V2 {
        return ProtocolVersion::V2;
    }
    let _ = requested;
    ProtocolVersion::V1
}

mod sealed {
    /// Prevents implementations of [`super::ConductorHostRole`] outside this crate.
    pub trait Sealed {}

    impl Sealed for agent_client_protocol::Agent {}
    impl Sealed for agent_client_protocol::Proxy {}
}

/// Conductor acting as an agent
impl ConductorHostRole for Agent {
    type Instantiator = Box<dyn InstantiateProxiesAndAgent>;

    fn initialize_method(&self) -> &'static str {
        "initialize"
    }

    /// Like an agent, select the highest supported version at or below the
    /// requested one, keeping v1's fallback-to-latest rule for a request below
    /// v1.
    fn select_protocol_version(
        &self,
        requested: ProtocolVersion,
    ) -> Result<ProtocolVersion, Error> {
        Ok(select_like_an_agent(requested))
    }

    async fn instantiate(
        &self,
        request: SelectedInitializeRequest,
        client_connection: ConnectionTo<Client>,
        instantiator: Self::Instantiator,
        runner: &mut ConductorRunner<Self>,
    ) -> Result<SelectedInitializeRequest, agent_client_protocol::Error> {
        let (modified_req, proxy_components, agent_component) =
            instantiator.instantiate_proxies_and_agent(request).await?;

        debug!(?agent_component, "spawning agent");

        // Like the client connection, the agent connection forwards every
        // protocol version untyped and leaves validation to the endpoints.
        let connection_to_agent = client_connection.spawn_connection(
            Builder::new(Client)
                .without_acp_version_guard()
                .name("conductor-to-agent")
                .on_receive_dispatch(
                    {
                        let mut conductor_tx = runner.conductor_tx.clone();
                        async move |dispatch: Dispatch, _cx| {
                            conductor_tx
                                .send(ConductorMessage::RightToLeft {
                                    source_component_index: SourceComponentIndex::Successor,
                                    message: dispatch,
                                })
                                .await
                                .map_err(agent_client_protocol::util::internal_error)
                        }
                    },
                    agent_client_protocol::on_receive_dispatch!(),
                ),
            agent_component,
        )?;
        runner.successor = Arc::new(connection_to_agent);

        runner.spawn_proxies(client_connection, proxy_components)?;

        Ok(modified_req)
    }

    async fn handle_dispatch(
        &self,
        message: Dispatch,
        client_connection: ConnectionTo<Client>,
        conductor_tx: &mut mpsc::Sender<ConductorMessage>,
    ) -> Result<Handled<Dispatch>, agent_client_protocol::Error> {
        tracing::debug!(
            method = ?message.method(),
            "ConductorToClient::handle_dispatch"
        );
        MatchDispatchFrom::new(message, &client_connection)
            // Any incoming messages from the client are client-to-agent messages targeting the first component.
            .if_dispatch_from(Client, async move |message: Dispatch| {
                tracing::debug!(
                    method = ?message.method(),
                    "ConductorToClient::handle_dispatch - matched Client"
                );
                ConductorImpl::<Self>::incoming_message_from_client(conductor_tx, message).await
            })
            .await
            .done()
    }
}

/// Conductor acting as a proxy
impl ConductorHostRole for Proxy {
    type Instantiator = Box<dyn InstantiateProxies>;

    fn initialize_method(&self) -> &'static str {
        METHOD_INITIALIZE_PROXY
    }

    /// Like any proxy, negotiate `proxy/initialize` as an agent negotiates
    /// `initialize`: select the highest supported version at or below the
    /// requested one, keeping v1's fallback-to-latest rule below v1.
    fn select_protocol_version(
        &self,
        requested: ProtocolVersion,
    ) -> Result<ProtocolVersion, Error> {
        Ok(select_like_an_agent(requested))
    }

    async fn instantiate(
        &self,
        request: SelectedInitializeRequest,
        client_connection: ConnectionTo<Conductor>,
        instantiator: Self::Instantiator,
        runner: &mut ConductorRunner<Self>,
    ) -> Result<SelectedInitializeRequest, agent_client_protocol::Error> {
        let (modified_req, proxy_components) = instantiator.instantiate_proxies(request).await?;

        runner.successor = Arc::new(GrandSuccessor);
        runner.spawn_proxies(client_connection, proxy_components)?;

        Ok(modified_req)
    }

    async fn handle_dispatch(
        &self,
        message: Dispatch,
        client_connection: ConnectionTo<Conductor>,
        conductor_tx: &mut mpsc::Sender<ConductorMessage>,
    ) -> Result<Handled<Dispatch>, agent_client_protocol::Error> {
        tracing::debug!(
            method = ?message.method(),
            id = ?message.raw_id(),
            "ConductorToConductor::handle_dispatch"
        );
        MatchDispatchFrom::new(message, &client_connection)
            .if_dispatch_from(Agent, {
                // Messages from our successor arrive already unwrapped
                // (RemoteRoleStyle::Successor strips the SuccessorMessage envelope).
                async |message: Dispatch| {
                    tracing::debug!(
                        method = ?message.method(),
                        "ConductorToConductor::handle_dispatch - matched Agent"
                    );
                    let mut conductor_tx = conductor_tx.clone();
                    ConductorImpl::<Self>::incoming_message_from_agent(&mut conductor_tx, message)
                        .await
                }
            })
            .await
            // Any incoming messages from the client are client-to-agent messages targeting the first component.
            .if_dispatch_from(Client, async |message: Dispatch| {
                tracing::debug!(
                    method = ?message.method(),
                    "ConductorToConductor::handle_dispatch - matched Client"
                );
                // A proxy is initialized with `proxy/initialize`, never with
                // a plain `initialize` from its predecessor.
                let message = match message {
                    Dispatch::Request(request, responder) if request.method() == "initialize" => {
                        return responder.respond_with_error(Error::invalid_request().data(
                            format!("proxies must be initialized with `{METHOD_INITIALIZE_PROXY}`"),
                        ));
                    }
                    message => message,
                };
                let mut conductor_tx = conductor_tx.clone();
                ConductorImpl::<Self>::incoming_message_from_client(&mut conductor_tx, message)
                    .await
            })
            .await
            .done()
    }
}

pub trait ConductorSuccessor<Host: ConductorHostRole>: Send + Sync + 'static {
    /// Send a message to the successor.
    fn send_message<'a>(
        &self,
        message: Dispatch,
        connection_to_conductor: ConnectionTo<Host::Counterpart>,
        runner: &'a mut ConductorRunner<Host>,
    ) -> BoxFuture<'a, Result<(), agent_client_protocol::Error>>;
}

impl<Host: ConductorHostRole> ConductorSuccessor<Host> for agent_client_protocol::Error {
    fn send_message<'a>(
        &self,
        _message: Dispatch,
        _connection_to_conductor: ConnectionTo<Host::Counterpart>,
        _runner: &'a mut ConductorRunner<Host>,
    ) -> BoxFuture<'a, Result<(), agent_client_protocol::Error>> {
        let error = self.clone();
        Box::pin(std::future::ready(Err(error)))
    }
}

/// A dummy type handling messages sent to the conductor's
/// successor when it is acting as a proxy.
struct GrandSuccessor;

/// When the conductor is acting as an proxy, messages sent by
/// the last proxy go to the conductor's successor.
///
/// ```text
/// client --> Conductor -----------------------------> GrandSuccessor
///            |                                  |
///            +-> Proxy[0] -> ... -> Proxy[n-1] -+
/// ```
impl ConductorSuccessor<Proxy> for GrandSuccessor {
    fn send_message<'a>(
        &self,
        message: Dispatch,
        connection: ConnectionTo<Conductor>,
        _runner: &'a mut ConductorRunner<Proxy>,
    ) -> BoxFuture<'a, Result<(), agent_client_protocol::Error>> {
        Box::pin(async move {
            debug!("Proxy mode: forwarding successor message to conductor's successor");
            connection.send_proxied_message_to(Agent, message)
        })
    }
}

/// When the conductor is acting as an agent, messages sent by
/// the last proxy to its successor go to the internal agent
/// (`self`).
impl ConductorSuccessor<Agent> for ConnectionTo<Agent> {
    fn send_message<'a>(
        &self,
        message: Dispatch,
        _connection: ConnectionTo<Client>,
        _runner: &'a mut ConductorRunner<Agent>,
    ) -> BoxFuture<'a, Result<(), agent_client_protocol::Error>> {
        debug!("Agent mode: forwarding successor message to the agent");
        Box::pin(std::future::ready(forward_message_to_agent(message, self)))
    }
}
