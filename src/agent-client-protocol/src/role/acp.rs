use std::{fmt::Debug, hash::Hash};

#[cfg(feature = "unstable_protocol_v2")]
use futures::{StreamExt as _, future};
#[cfg(feature = "unstable_protocol_v2")]
use serde::{Serialize, de::DeserializeOwned};

#[cfg(feature = "unstable_protocol_v2")]
use crate::DynConnectTo;
use crate::jsonrpc::{Builder, handlers::NullHandler, run::NullRun};
#[cfg(feature = "unstable_protocol_v2")]
use crate::jsonrpc::{
    TransportBatch, TransportBatchEntry, TransportFrame, V2Builder, is_response_only_shape,
    raw_is_response_only_shape,
};
use crate::role::{HasPeer, RemoteStyle};
#[cfg(not(feature = "unstable_protocol_v2"))]
use crate::schema::InitializeProxyRequest;
use crate::schema::METHOD_INITIALIZE_PROXY;
use crate::schema::v1::{InitializeRequest, SessionId};
#[cfg(not(feature = "unstable_protocol_v2"))]
use crate::schema::v1::{NewSessionRequest, NewSessionResponse};
#[cfg(feature = "unstable_protocol_v2")]
use crate::schema::v1::{RequestId, Response as RpcResponse};
#[cfg(feature = "unstable_protocol_v2")]
use crate::schema::{ProtocolVersion, v2};
use crate::util::MatchDispatchFrom;
#[cfg(feature = "unstable_protocol_v2")]
use crate::{Channel, RawJsonRpcMessage, RawJsonRpcParams};
use crate::{ConnectTo, ConnectionTo, Dispatch, HandleDispatchFrom, Handled, Role, RoleId};

#[cfg(feature = "unstable_protocol_v2")]
#[derive(serde::Deserialize)]
struct NewSessionResponseEnvelope {
    #[serde(rename = "sessionId")]
    session_id: SessionId,
}

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
    pub fn v2(self) -> V2Builder<Client, NullHandler, NullRun> {
        self.builder().v2_client()
    }

    /// Create a connector that chooses between configured protocol implementations.
    ///
    /// Add implementation factories with [`ClientProtocolConnector::with_v1`]
    /// and [`ClientProtocolConnector::with_v2`]. The resulting connector starts
    /// the highest configured protocol implementation. If a v2 implementation
    /// successfully negotiates v1 and a v1 implementation is configured, the
    /// connector reuses the connection only when the v1 implementation's
    /// complete `initialize` parameters match what the agent already saw;
    /// otherwise it opens a fresh agent connection and restarts with v1.
    ///
    /// Requires the `unstable_protocol_v2` crate feature while protocol v2
    /// stabilizes.
    #[cfg(feature = "unstable_protocol_v2")]
    #[must_use]
    pub fn protocol_connector(self) -> ClientProtocolConnector {
        ClientProtocolConnector::new()
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

/// Client connector that opens an agent connection with a configured protocol implementation.
///
/// Use [`Client::protocol_connector`] to start the builder, then add each
/// supported protocol version independently. Implementations and the agent
/// connection are provided as factories because fallback from v2 to v1 may
/// require a fresh connection initialized by the v1 implementation.
#[cfg(feature = "unstable_protocol_v2")]
#[derive(Debug, Default)]
pub struct ClientProtocolConnector {
    v1: Option<DynConnectToFactory<Agent>>,
    v2: Option<DynConnectToFactory<Agent>>,
}

#[cfg(feature = "unstable_protocol_v2")]
impl ClientProtocolConnector {
    /// Create an empty client protocol connector.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return this connector with an ACP v1 implementation factory configured.
    #[must_use]
    pub fn with_v1<C>(mut self, client: impl FnMut() -> C + Send + 'static) -> Self
    where
        C: ConnectTo<Agent>,
    {
        self.v1 = Some(DynConnectToFactory::new(client));
        self
    }

    /// Return this connector with an ACP v2 implementation factory configured.
    #[must_use]
    pub fn with_v2<C>(mut self, client: impl FnMut() -> C + Send + 'static) -> Self
    where
        C: ConnectTo<Agent>,
    {
        self.v2 = Some(DynConnectToFactory::new(client));
        self
    }

    /// Connect to an agent produced by `agent` using the highest configured
    /// compatible protocol implementation.
    pub async fn connect_to<C>(
        mut self,
        mut agent: impl FnMut() -> C + Send + 'static,
    ) -> Result<(), crate::Error>
    where
        C: ConnectTo<Client>,
    {
        let supported = SupportedClientProtocols {
            v1: self.v1.is_some(),
            v2: self.v2.is_some(),
        };
        let Some(selected) = supported.highest_configured() else {
            return Err(crate::Error::invalid_request()
                .data("client protocol connector has no configured ACP protocol implementations"));
        };

        match selected {
            ClientProtocol::V1 => {
                let client = self
                    .v1
                    .as_mut()
                    .expect("selected protocol is configured")
                    .create();
                connect_client_protocol(ClientProtocol::V1, client, agent()).await
            }
            ClientProtocol::V2 => {
                let client = self
                    .v2
                    .as_mut()
                    .expect("selected protocol is configured")
                    .create();
                let agent_connection = RunningProtocolPeer::new(agent());
                let (client, initialize) =
                    start_client_protocol(ClientProtocol::V2, client).await?;
                // This normalization is only a probe for the connection-reuse
                // optimization. A request that cannot be represented in v1
                // can still be valid v2 traffic and must reach the agent.
                let v2_initialize_as_v1 = normalize_v2_initialize_params_for_reuse(&initialize);
                let (client, agent_connection, initialize_response) =
                    send_initialize_and_receive(client, agent_connection, initialize).await?;

                if initialize_response_negotiated_v1(&initialize_response)
                    && let Some(v1) = self.v1.as_mut()
                {
                    let fallback_client = v1.create();
                    let (fallback_client, fallback_initialize) =
                        start_client_protocol(ClientProtocol::V1, fallback_client).await?;
                    let v1_initialize =
                        validated_initialize_params::<InitializeRequest>(&fallback_initialize)?;

                    if v2_initialize_as_v1
                        .as_ref()
                        .is_ok_and(|v2_initialize| v2_initialize == &v1_initialize)
                    {
                        let fallback_response = initialize_response.with_id(
                            initialize_request_id(&fallback_initialize)
                                .expect("validated initialize request has an id"),
                        );
                        // The v2 implementation will never receive its initialize response once
                        // the matching v1 implementation takes over this connection. Drop its
                        // future and channels before running the v1 session so any resources it
                        // owns are released promptly.
                        drop(client);
                        fallback_client.send(fallback_response)?;
                        return pipe_protocol_peers_until_done(fallback_client, agent_connection)
                            .await;
                    }

                    // Neither probe can continue on the replacement connection. Release both
                    // client implementations and the original agent connection before starting
                    // the real v1 session so they cannot retain resources for its lifetime.
                    drop((
                        client,
                        fallback_client,
                        agent_connection,
                        initialize_response,
                    ));
                    return connect_client_protocol(ClientProtocol::V1, v1.create(), agent()).await;
                }

                client.send(initialize_response.into_message())?;
                pipe_protocol_peers_until_done(client, agent_connection).await
            }
        }
    }
}

#[cfg(feature = "unstable_protocol_v2")]
struct DynConnectToFactory<R: Role> {
    inner: Box<dyn FnMut() -> DynConnectTo<R> + Send>,
}

#[cfg(feature = "unstable_protocol_v2")]
impl<R: Role> DynConnectToFactory<R> {
    fn new<C>(mut factory: impl FnMut() -> C + Send + 'static) -> Self
    where
        C: ConnectTo<R>,
    {
        Self {
            inner: Box::new(move || DynConnectTo::new(factory())),
        }
    }

    fn create(&mut self) -> DynConnectTo<R> {
        (self.inner)()
    }
}

#[cfg(feature = "unstable_protocol_v2")]
impl<R: Role> Debug for DynConnectToFactory<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynConnectToFactory")
            .finish_non_exhaustive()
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
            .if_dispatch_from(Agent, async |message: Dispatch| {
                // Stable v1 session helpers install a dynamic handler after
                // `session/new`. Retry session messages to close the race
                // between the response and that registration.
                //
                // V2 uses typed handlers installed before the connection
                // starts. Retrying an unhandled v2 message would retain it
                // forever because no per-session dynamic handler is expected.
                #[cfg(feature = "unstable_protocol_v2")]
                let retry = message.has_session_id()
                    && connection.acp_protocol_version()
                        != Some(crate::schema::ProtocolVersion::V2);
                #[cfg(not(feature = "unstable_protocol_v2"))]
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
    pub fn v2(self) -> V2Builder<Agent, NullHandler, NullRun> {
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
        let supported = SupportedProtocols {
            v1: self.v1.is_some(),
            v2: self.v2.is_some(),
        };
        let mut client = RunningProtocolPeer::new(client);
        let (first_frame, client, selected) = loop {
            let Some((mut frame, next_client)) = client.next_frame().await? else {
                return Ok(());
            };
            let message = match initialize_message_mut(&mut frame) {
                Ok(Some(message)) => message,
                Ok(None) => {
                    client = next_client;
                    continue;
                }
                Err(error) => return reject_initialize(next_client, &frame, error).await,
            };
            let selected = match select_agent_protocol(message, supported) {
                Ok(selected) => selected,
                Err(error) => return reject_initialize(next_client, &frame, error).await,
            };
            break (frame, next_client, selected);
        };
        let Some(agent) = selected.take_agent(self) else {
            let error = selected.unsupported_error(supported);
            return reject_initialize(client, &first_frame, error).await;
        };

        let agent = RunningProtocolPeer::new(agent);
        agent.send_frame(first_frame)?;
        pipe_protocol_peers_until_closed(client, agent).await
    }
}

#[cfg(feature = "unstable_protocol_v2")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectedProtocol {
    V1,
    V2,
}

#[cfg(feature = "unstable_protocol_v2")]
impl SelectedProtocol {
    fn take_agent(self, agent: AgentProtocolRouter) -> Option<DynConnectTo<Client>> {
        match self {
            Self::V1 => agent.v1,
            Self::V2 => agent.v2,
        }
    }

    fn version(self) -> ProtocolVersion {
        match self {
            Self::V1 => ProtocolVersion::V1,
            Self::V2 => ProtocolVersion::V2,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::V1 => "1",
            Self::V2 => "2",
        }
    }

    fn unsupported_error(self, supported: SupportedProtocols) -> crate::Error {
        crate::Error::invalid_request().data(format!(
            "ACP protocol version {} is not configured; this endpoint supports {}",
            self.name(),
            supported.description()
        ))
    }
}

#[cfg(feature = "unstable_protocol_v2")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SupportedProtocols {
    v1: bool,
    v2: bool,
}

#[cfg(feature = "unstable_protocol_v2")]
impl SupportedProtocols {
    fn highest_compatible(self, requested: ProtocolVersion) -> Option<SelectedProtocol> {
        if self.v2 && requested >= ProtocolVersion::V2 {
            return Some(SelectedProtocol::V2);
        }

        if self.v1 && requested >= ProtocolVersion::V1 {
            return Some(SelectedProtocol::V1);
        }

        None
    }

    fn exact(self, requested: ProtocolVersion) -> Option<SelectedProtocol> {
        if self.v1 && requested == ProtocolVersion::V1 {
            Some(SelectedProtocol::V1)
        } else if self.v2 && requested == ProtocolVersion::V2 {
            Some(SelectedProtocol::V2)
        } else {
            None
        }
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
    supported: SupportedProtocols,
) -> Result<SelectedProtocol, crate::Error> {
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
    rewrite_initialize_params(params, requested, selected)?;

    Ok(selected)
}

#[cfg(feature = "unstable_protocol_v2")]
fn initialize_request_params(
    message: &RawJsonRpcMessage,
) -> Result<&serde_json::Map<String, serde_json::Value>, crate::Error> {
    let RawJsonRpcMessage::Request(request) = message else {
        return Err(
            crate::Error::invalid_request().data("first ACP message must be an initialize request")
        );
    };

    if request.method.as_ref() != "initialize" {
        return Err(crate::Error::invalid_request().data("first ACP request must be initialize"));
    }

    let Some(RawJsonRpcParams::Object(params)) = &request.params else {
        return Err(invalid_initialize_protocol_version());
    };
    if !params.contains_key("protocolVersion") {
        return Err(invalid_initialize_protocol_version());
    }
    Ok(params)
}

#[cfg(feature = "unstable_protocol_v2")]
fn validated_initialize_params<T: DeserializeOwned>(
    message: &RawJsonRpcMessage,
) -> Result<serde_json::Map<String, serde_json::Value>, crate::Error> {
    let params = initialize_request_params(message)?;
    parse_initialize_params::<T>(params)?;
    Ok(params.clone())
}

#[cfg(feature = "unstable_protocol_v2")]
fn normalize_v2_initialize_params_for_reuse(
    message: &RawJsonRpcMessage,
) -> Result<serde_json::Map<String, serde_json::Value>, crate::Error> {
    let params = initialize_request_params(message)?;
    let requested = params
        .get("protocolVersion")
        .cloned()
        .ok_or_else(invalid_initialize_protocol_version)
        .and_then(|version| {
            serde_json::from_value::<ProtocolVersion>(version)
                .map_err(|_| invalid_initialize_protocol_version())
        })?;
    if requested == ProtocolVersion::V1 {
        parse_initialize_params::<InitializeRequest>(params)?;
        return Ok(params.clone());
    }
    normalize_v2_initialize_params_for_v1(params, true)
}

#[cfg(feature = "unstable_protocol_v2")]
fn rewrite_initialize_params(
    params: &mut serde_json::Map<String, serde_json::Value>,
    requested: ProtocolVersion,
    selected: SelectedProtocol,
) -> Result<(), crate::Error> {
    // Validate exact-version initialization without replacing its raw
    // parameters. Reserializing through the SDK's pinned schema would discard
    // fields added by newer compatible peers.
    if requested == selected.version() {
        match selected {
            SelectedProtocol::V1 => {
                parse_initialize_params::<InitializeRequest>(params)?;
            }
            SelectedProtocol::V2 => {
                parse_initialize_params::<v2::InitializeRequest>(params)?;
            }
        }
        return Ok(());
    }

    match selected {
        SelectedProtocol::V1 => {
            debug_assert!(requested >= ProtocolVersion::V2);
            *params = normalize_v2_initialize_params_for_v1(params, false)?;
            Ok(())
        }
        SelectedProtocol::V2 => {
            let mut initialize = parse_initialize_params::<v2::InitializeRequest>(params)?;
            initialize.protocol_version = ProtocolVersion::V2;
            *params = serialize_initialize_params(initialize)?;
            Ok(())
        }
    }
}

#[cfg(feature = "unstable_protocol_v2")]
fn normalize_v2_initialize_params_for_v1(
    params: &serde_json::Map<String, serde_json::Value>,
    require_lossless: bool,
) -> Result<serde_json::Map<String, serde_json::Value>, crate::Error> {
    // Canonicalize through v2 first so tolerant field semantics are applied.
    // A lossless result is only required by the connection-reuse probe. Normal
    // v1 routing may discard fields that have no meaning in the selected
    // protocol version.
    let initialize = parse_initialize_params::<v2::InitializeRequest>(params)?;
    let mut target = serialize_initialize_params(initialize)?;
    if require_lossless && target != *params {
        return Err(invalid_initialize_params(
            "v2 initialize parameters are not losslessly representable in v1",
        ));
    }

    target.insert(
        "protocolVersion".into(),
        serde_json::to_value(ProtocolVersion::V1).map_err(crate::Error::into_internal_error)?,
    );
    let info = target
        .remove("info")
        .ok_or_else(|| invalid_initialize_params("v2 InitializeRequest.info is required"))?;
    target.insert("clientInfo".into(), info);
    let capabilities = target
        .remove("capabilities")
        .and_then(|capabilities| capabilities.as_object().cloned())
        .ok_or_else(|| {
            crate::util::internal_error("v2 initialize capabilities did not serialize as an object")
        })?;
    let mut capabilities = capabilities;
    if let Some(auth) = capabilities
        .get_mut("auth")
        .and_then(serde_json::Value::as_object_mut)
    {
        let terminal = auth.remove("terminal");
        if require_lossless
            && terminal
                .as_ref()
                .and_then(serde_json::Value::as_object)
                .is_some_and(|terminal| terminal.contains_key("_meta"))
        {
            return Err(invalid_initialize_params(
                "v2 terminal authentication metadata is not representable in v1",
            ));
        }
        auth.insert("terminal".into(), terminal.is_some().into());
    }
    capabilities.insert(
        "session".into(),
        serde_json::json!({ "configOptions": { "boolean": {} } }),
    );
    target.insert("clientCapabilities".into(), capabilities.into());

    let initialize = parse_initialize_params::<InitializeRequest>(&target)?;
    let normalized = serialize_initialize_params(initialize)?;
    if require_lossless && !json_object_contains(&normalized, &target) {
        return Err(invalid_initialize_params(
            "v2 initialize parameters are not losslessly representable in v1",
        ));
    }
    Ok(normalized)
}

#[cfg(all(test, feature = "unstable_protocol_v2"))]
mod initialize_normalization_tests {
    use super::*;

    fn v2_initialize_params() -> serde_json::Map<String, serde_json::Value> {
        let value = serde_json::to_value(v2::InitializeRequest::new(
            ProtocolVersion::V2,
            v2::Implementation::new("test-client", "1.0.0"),
        ))
        .expect("serialize v2 initialize request");
        value
            .as_object()
            .expect("initialize params serialize as an object")
            .clone()
    }

    #[test]
    fn v2_tolerant_fields_are_canonicalized_before_v1_normalization() {
        let mut params = v2_initialize_params();
        params.insert(
            "capabilities".into(),
            serde_json::Value::String("malformed".into()),
        );
        params.insert(
            "_meta".into(),
            serde_json::Value::String("malformed".into()),
        );

        let normalized = normalize_v2_initialize_params_for_v1(&params, false)
            .expect("tolerant v2 fields should normalize through their defaults");
        let normalized = serde_json::Value::Object(normalized);

        assert!(normalized.get("_meta").is_none());
        assert_eq!(
            normalized.pointer("/clientCapabilities/session/configOptions/boolean"),
            Some(&serde_json::json!({}))
        );
    }

    #[test]
    fn noncanonical_v2_fields_disable_reuse_but_not_v1_routing() {
        let mut params = v2_initialize_params();
        params
            .get_mut("info")
            .and_then(serde_json::Value::as_object_mut)
            .expect("v2 initialize info is an object")
            .insert("buildCommit".into(), serde_json::json!("abc123"));

        normalize_v2_initialize_params_for_v1(&params, false)
            .expect("v1 routing may ignore parameters unavailable in v1");
        normalize_v2_initialize_params_for_v1(&params, true)
            .expect_err("connection reuse requires lossless normalization");
    }

    #[test]
    fn v1_reuse_probe_preserves_raw_initialize_params() {
        let mut params = v2_initialize_params();
        params.insert(
            "protocolVersion".into(),
            serde_json::json!(ProtocolVersion::V1),
        );
        let message = RawJsonRpcMessage::request(
            "initialize".into(),
            serde_json::Value::Object(params.clone()),
            RequestId::Number(1),
        )
        .expect("build initialize request");

        let normalized = normalize_v2_initialize_params_for_reuse(&message)
            .expect("v1-shaped initialize request should be valid");

        assert_eq!(normalized, params);
    }

    #[cfg(feature = "unstable_auth_methods")]
    #[test]
    fn null_v2_terminal_marker_meta_is_omitted_before_v1_normalization() {
        let mut params = v2_initialize_params();
        params.insert(
            "capabilities".into(),
            serde_json::json!({
                "auth": {
                    "terminal": { "_meta": null }
                }
            }),
        );

        let normalized = normalize_v2_initialize_params_for_v1(&params, false)
            .expect("null marker metadata is equivalent to omission");
        let normalized = serde_json::Value::Object(normalized);

        assert_eq!(
            normalized.pointer("/clientCapabilities/auth/terminal"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[cfg(feature = "unstable_auth_methods")]
    #[test]
    fn terminal_marker_metadata_disables_reuse_but_not_v1_routing() {
        let mut params = v2_initialize_params();
        params.insert(
            "capabilities".into(),
            serde_json::json!({
                "auth": {
                    "terminal": {
                        "_meta": { "source": "test" }
                    }
                }
            }),
        );

        normalize_v2_initialize_params_for_v1(&params, false)
            .expect("v1 routing may discard terminal marker metadata");
        normalize_v2_initialize_params_for_v1(&params, true)
            .expect_err("connection reuse must preserve terminal marker metadata");
    }
}

#[cfg(feature = "unstable_protocol_v2")]
fn parse_initialize_params<T: DeserializeOwned>(
    params: &serde_json::Map<String, serde_json::Value>,
) -> Result<T, crate::Error> {
    serde_json::from_value(serde_json::Value::Object(params.clone()))
        .map_err(invalid_initialize_params)
}

#[cfg(feature = "unstable_protocol_v2")]
fn serialize_initialize_params(
    initialize: impl Serialize,
) -> Result<serde_json::Map<String, serde_json::Value>, crate::Error> {
    let value = serde_json::to_value(initialize).map_err(crate::Error::into_internal_error)?;
    let serde_json::Value::Object(object) = value else {
        return Err(crate::util::internal_error(
            "initialize params did not serialize to an object",
        ));
    };
    Ok(object)
}

#[cfg(feature = "unstable_protocol_v2")]
fn json_object_contains(
    actual: &serde_json::Map<String, serde_json::Value>,
    expected: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    fn contains(actual: &serde_json::Value, expected: &serde_json::Value) -> bool {
        match (actual, expected) {
            (serde_json::Value::Object(actual), serde_json::Value::Object(expected)) => expected
                .iter()
                .all(|(key, value)| actual.get(key).is_some_and(|item| contains(item, value))),
            _ => actual == expected,
        }
    }

    expected
        .iter()
        .all(|(key, value)| actual.get(key).is_some_and(|item| contains(item, value)))
}

#[cfg(feature = "unstable_protocol_v2")]
fn highest_compatible_agent_protocol(
    requested: ProtocolVersion,
    supported: SupportedProtocols,
) -> Result<SelectedProtocol, crate::Error> {
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
fn invalid_initialize_params(error: impl ToString) -> crate::Error {
    crate::Error::invalid_params().data(format!("invalid initialize params: {}", error.to_string()))
}

#[cfg(feature = "unstable_protocol_v2")]
fn send_initialize_error(
    tx: &futures::channel::mpsc::UnboundedSender<TransportFrame>,
    frame: &TransportFrame,
    error: crate::Error,
) -> Result<(), crate::Error> {
    fn response_for_message(
        entry: &RawJsonRpcMessage,
        initialize_error: &crate::Error,
    ) -> Option<RawJsonRpcMessage> {
        match entry {
            RawJsonRpcMessage::Request(request) => Some(RawJsonRpcMessage::response(
                request.id.clone(),
                Err(initialize_error.clone()),
            )),
            RawJsonRpcMessage::Notification(_) | RawJsonRpcMessage::Response(_) => None,
        }
    }

    fn response_for_entry(
        entry: &TransportBatchEntry,
        initialize_error: &crate::Error,
    ) -> Option<RawJsonRpcMessage> {
        match entry {
            TransportBatchEntry::Message(message) => {
                response_for_message(message, initialize_error)
            }
            TransportBatchEntry::Malformed { raw, error } if !is_response_only_shape(raw) => Some(
                RawJsonRpcMessage::response(RequestId::Null, Err(error.clone())),
            ),
            TransportBatchEntry::Malformed { .. } => None,
        }
    }

    let response = match frame {
        TransportFrame::Single(entry) => {
            let Some(response) = response_for_message(entry, &error) else {
                return Ok(());
            };
            TransportFrame::Single(response)
        }
        TransportFrame::Malformed { raw, error } if !raw_is_response_only_shape(raw) => {
            TransportFrame::Single(RawJsonRpcMessage::response(
                RequestId::Null,
                Err(error.clone()),
            ))
        }
        TransportFrame::Malformed { .. } => return Ok(()),
        TransportFrame::Batch(batch) => {
            let responses = batch
                .entries()
                .filter_map(|entry| response_for_entry(entry, &error))
                .collect::<Vec<_>>();
            let Some(responses) = TransportBatch::from_messages(responses) else {
                return Ok(());
            };
            TransportFrame::Batch(responses)
        }
    };

    tx.unbounded_send(response)
        .map_err(crate::util::internal_error)
}

#[cfg(feature = "unstable_protocol_v2")]
async fn reject_initialize(
    client: RunningProtocolPeer,
    frame: &TransportFrame,
    error: crate::Error,
) -> Result<(), crate::Error> {
    let RunningProtocolPeer { mut rx, tx, future } = client;
    send_initialize_error(&tx, frame, error)?;
    drop(tx);

    let drain_incoming = async move {
        // Later input has no protocol meaning once initialization is rejected.
        // Keep draining it only so the transport can flush the queued rejection;
        // treating a malformed trailing frame as fatal would cancel that flush.
        while rx.next().await.is_some() {}
        Ok::<_, crate::Error>(())
    };

    let ((), ()) = futures::try_join!(future, drain_incoming)?;
    Ok(())
}

#[cfg(feature = "unstable_protocol_v2")]
struct RunningProtocolPeer {
    rx: futures::channel::mpsc::UnboundedReceiver<TransportFrame>,
    tx: futures::channel::mpsc::UnboundedSender<TransportFrame>,
    future: crate::BoxFuture<'static, Result<(), crate::Error>>,
}

#[cfg(feature = "unstable_protocol_v2")]
impl RunningProtocolPeer {
    fn new<R: Role>(component: impl ConnectTo<R>) -> Self {
        let (Channel { rx, tx }, future) = component.into_channel_and_future();
        Self { rx, tx, future }
    }

    async fn next_frame(self) -> Result<Option<(TransportFrame, Self)>, crate::Error> {
        let Self { mut rx, tx, future } = self;
        match future::select(Box::pin(rx.next()), future).await {
            future::Either::Left((Some(frame), future)) => {
                Ok(Some((frame, Self { rx, tx, future })))
            }
            future::Either::Left((None, future)) => {
                future.await?;
                Ok(None)
            }
            future::Either::Right((result, next_message)) => {
                result?;
                drop(next_message);
                let Some(frame) = rx.next().await else {
                    return Ok(None);
                };
                Ok(Some((
                    frame,
                    Self {
                        rx,
                        tx,
                        future: Box::pin(future::ready(Ok(()))),
                    },
                )))
            }
        }
    }

    async fn next_message(self) -> Result<Option<(RawJsonRpcMessage, Self)>, crate::Error> {
        let Some((frame, peer)) = self.next_frame().await? else {
            return Ok(None);
        };
        Ok(Some((initialize_message(frame)?, peer)))
    }

    fn send(&self, message: RawJsonRpcMessage) -> Result<(), crate::Error> {
        self.send_frame(TransportFrame::Single(message))
    }

    fn send_frame(&self, frame: TransportFrame) -> Result<(), crate::Error> {
        self.tx
            .unbounded_send(frame)
            .map_err(crate::util::internal_error)
    }
}

#[cfg(feature = "unstable_protocol_v2")]
fn initialize_message(frame: TransportFrame) -> Result<RawJsonRpcMessage, crate::Error> {
    match frame {
        TransportFrame::Single(message) => Ok(message),
        TransportFrame::Malformed { error, .. } => Err(error),
        TransportFrame::Batch(_) => Err(crate::Error::invalid_request()
            .data("ACP initialize request and response messages must be sent individually")),
    }
}

#[cfg(feature = "unstable_protocol_v2")]
fn initialize_message_mut(
    frame: &mut TransportFrame,
) -> Result<Option<&mut RawJsonRpcMessage>, crate::Error> {
    match frame {
        TransportFrame::Single(RawJsonRpcMessage::Response(_)) => Ok(None),
        TransportFrame::Single(entry) => Ok(Some(entry)),
        TransportFrame::Malformed { raw, .. } if raw_is_response_only_shape(raw) => Ok(None),
        TransportFrame::Malformed { error, .. } => Err(error.clone()),
        TransportFrame::Batch(batch) => {
            for entry in batch.entries_mut() {
                match entry {
                    TransportBatchEntry::Message(RawJsonRpcMessage::Response(_)) => {}
                    TransportBatchEntry::Message(message) => return Ok(Some(message)),
                    TransportBatchEntry::Malformed { raw, .. } if is_response_only_shape(raw) => {}
                    TransportBatchEntry::Malformed { error, .. } => return Err(error.clone()),
                }
            }
            Ok(None)
        }
    }
}

#[cfg(feature = "unstable_protocol_v2")]
async fn pipe_protocol_peers_until_closed(
    left: RunningProtocolPeer,
    right: RunningProtocolPeer,
) -> Result<(), crate::Error> {
    let ((), (), (), ()) = futures::try_join!(
        left.future,
        right.future,
        Channel {
            rx: left.rx,
            tx: right.tx,
        }
        .copy(),
        Channel {
            rx: right.rx,
            tx: left.tx,
        }
        .copy(),
    )?;

    Ok(())
}

#[cfg(feature = "unstable_protocol_v2")]
async fn pipe_protocol_peers_until_done(
    left: RunningProtocolPeer,
    right: RunningProtocolPeer,
) -> Result<(), crate::Error> {
    let bridge = Box::pin(async move {
        let ((), ()) = futures::try_join!(
            Channel {
                rx: left.rx,
                tx: right.tx,
            }
            .copy(),
            Channel {
                rx: right.rx,
                tx: left.tx,
            }
            .copy(),
        )?;
        Ok(())
    });

    match future::select(left.future, future::select(right.future, bridge)).await {
        future::Either::Left((result, _))
        | future::Either::Right((
            future::Either::Left((result, _)) | future::Either::Right((result, _)),
            _,
        )) => result,
    }
}

#[cfg(feature = "unstable_protocol_v2")]
#[derive(Debug)]
struct InitializeResponse {
    id: RequestId,
    result: Result<serde_json::Value, crate::Error>,
}

#[cfg(feature = "unstable_protocol_v2")]
impl InitializeResponse {
    fn from_message(message: RawJsonRpcMessage) -> Result<Self, crate::Error> {
        match message {
            RawJsonRpcMessage::Response(RpcResponse::Result { id, result }) => Ok(Self {
                id,
                result: Ok(result),
            }),
            RawJsonRpcMessage::Response(RpcResponse::Error { id, error }) => Ok(Self {
                id,
                result: Err(error),
            }),
            message => Err(crate::Error::invalid_request().data(format!(
                "first ACP response must be an initialize response, got {message:?}",
            ))),
        }
    }

    fn into_message(self) -> RawJsonRpcMessage {
        RawJsonRpcMessage::response(self.id, self.result)
    }

    fn with_id(self, id: RequestId) -> RawJsonRpcMessage {
        RawJsonRpcMessage::response(id, self.result)
    }

    fn protocol_version(&self) -> Option<ProtocolVersion> {
        serde_json::from_value(self.result.as_ref().ok()?.get("protocolVersion")?.clone()).ok()
    }
}

#[cfg(feature = "unstable_protocol_v2")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientProtocol {
    V1,
    V2,
}

#[cfg(feature = "unstable_protocol_v2")]
impl ClientProtocol {
    fn name(self) -> &'static str {
        match self {
            Self::V1 => "1",
            Self::V2 => "2",
        }
    }
}

#[cfg(feature = "unstable_protocol_v2")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SupportedClientProtocols {
    v1: bool,
    v2: bool,
}

#[cfg(feature = "unstable_protocol_v2")]
impl SupportedClientProtocols {
    fn highest_configured(self) -> Option<ClientProtocol> {
        if self.v2 {
            return Some(ClientProtocol::V2);
        }

        if self.v1 {
            return Some(ClientProtocol::V1);
        }

        None
    }
}

#[cfg(feature = "unstable_protocol_v2")]
async fn start_client_protocol(
    protocol: ClientProtocol,
    client: DynConnectTo<Agent>,
) -> Result<(RunningProtocolPeer, RawJsonRpcMessage), crate::Error> {
    let client = RunningProtocolPeer::new(client);
    let Some((initialize, client)) = client.next_message().await? else {
        return Err(crate::Error::invalid_request().data(format!(
            "ACP protocol version {} client implementation ended before initialize",
            protocol.name()
        )));
    };
    ensure_client_initialize_request(protocol, &initialize)?;
    Ok((client, initialize))
}

#[cfg(feature = "unstable_protocol_v2")]
async fn send_initialize_and_receive(
    client: RunningProtocolPeer,
    agent: RunningProtocolPeer,
    initialize: RawJsonRpcMessage,
) -> Result<(RunningProtocolPeer, RunningProtocolPeer, InitializeResponse), crate::Error> {
    agent.send(initialize)?;
    let Some((response, agent)) = agent.next_message().await? else {
        return Err(crate::Error::internal_error().data("agent closed before initialize response"));
    };
    let response = InitializeResponse::from_message(response)?;
    Ok((client, agent, response))
}

#[cfg(feature = "unstable_protocol_v2")]
async fn initialize_client_protocol(
    protocol: ClientProtocol,
    client: DynConnectTo<Agent>,
    agent: impl ConnectTo<Client>,
) -> Result<(RunningProtocolPeer, RunningProtocolPeer, InitializeResponse), crate::Error> {
    let agent = RunningProtocolPeer::new(agent);
    let (client, initialize) = start_client_protocol(protocol, client).await?;
    send_initialize_and_receive(client, agent, initialize).await
}

#[cfg(feature = "unstable_protocol_v2")]
async fn connect_client_protocol(
    protocol: ClientProtocol,
    client: DynConnectTo<Agent>,
    agent: impl ConnectTo<Client>,
) -> Result<(), crate::Error> {
    let (client, agent, initialize_response) =
        initialize_client_protocol(protocol, client, agent).await?;
    client.send(initialize_response.into_message())?;
    pipe_protocol_peers_until_done(client, agent).await
}

#[cfg(feature = "unstable_protocol_v2")]
fn ensure_client_initialize_request(
    protocol: ClientProtocol,
    message: &RawJsonRpcMessage,
) -> Result<(), crate::Error> {
    let RawJsonRpcMessage::Request(request) = message else {
        return Err(crate::Error::invalid_request().data(format!(
            "ACP protocol version {} client implementation must send initialize first",
            protocol.name()
        )));
    };

    if request.method.as_ref() != "initialize" {
        return Err(crate::Error::invalid_request().data(format!(
            "ACP protocol version {} client implementation must send initialize first",
            protocol.name()
        )));
    }

    Ok(())
}

#[cfg(feature = "unstable_protocol_v2")]
fn initialize_request_id(message: &RawJsonRpcMessage) -> Option<RequestId> {
    let RawJsonRpcMessage::Request(request) = message else {
        return None;
    };
    Some(request.id.clone())
}

#[cfg(feature = "unstable_protocol_v2")]
fn initialize_response_negotiated_v1(response: &InitializeResponse) -> bool {
    response.protocol_version() == Some(ProtocolVersion::V1)
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
    /// Create a stable protocol v1 connection builder for a proxy.
    ///
    /// Use `Proxy::v2` for a protocol-v2-only proxy with typed callbacks and
    /// wire validation. Protocol-routing infrastructure that deliberately
    /// selects a version itself can disable the guard with
    /// `Builder::without_acp_version_guard`.
    pub fn builder(self) -> Builder<Proxy, NullHandler, NullRun> {
        Builder::new(self)
    }

    /// Create a proxy builder that uses the ACP protocol v2 API.
    ///
    /// This builder requires `_proxy/initialize` to select protocol v2.
    /// Fluent callbacks receive [`crate::V2ConnectionTo<Conductor>`], while
    /// low-level custom handlers and runners retain the protocol-neutral
    /// [`ConnectionTo`] interface.
    ///
    /// Requires the `unstable_protocol_v2` crate feature.
    #[cfg(feature = "unstable_protocol_v2")]
    pub fn v2(self) -> V2Builder<Proxy, NullHandler, NullRun> {
        self.builder().v2_proxy()
    }

    /// Create a router that chooses between configured proxy implementations.
    ///
    /// Add implementations with [`ProxyProtocolRouter::with_v1`] and
    /// [`ProxyProtocolRouter::with_v2`]. The router reads the initial
    /// `_proxy/initialize` request, selects the implementation for that exact
    /// protocol version, and hands over the complete initial transport frame.
    /// It does not downgrade proxy traffic or convert later messages.
    ///
    /// Requires the `unstable_protocol_v2` crate feature while protocol v2
    /// stabilizes.
    #[cfg(feature = "unstable_protocol_v2")]
    #[must_use]
    pub fn protocol_router(self) -> ProxyProtocolRouter {
        ProxyProtocolRouter::new()
    }
}

/// Proxy component that routes each connection to a configured protocol implementation.
///
/// Use [`Proxy::protocol_router`] to start the builder, then add stable-v1 and
/// draft-v2 proxy implementations independently. Unlike
/// [`AgentProtocolRouter`], this router requires an exact version match: the
/// conductor has already selected and canonicalized the wire protocol before
/// sending `_proxy/initialize` to a proxy.
#[cfg(feature = "unstable_protocol_v2")]
#[derive(Debug, Default)]
pub struct ProxyProtocolRouter {
    v1: Option<DynConnectTo<Conductor>>,
    v2: Option<DynConnectTo<Conductor>>,
}

#[cfg(feature = "unstable_protocol_v2")]
impl ProxyProtocolRouter {
    /// Create an empty proxy protocol router.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return this router with a stable ACP v1 proxy implementation.
    #[must_use]
    pub fn with_v1(mut self, proxy: impl ConnectTo<Conductor>) -> Self {
        self.v1 = Some(DynConnectTo::new(proxy));
        self
    }

    /// Return this router with a draft ACP v2 proxy implementation.
    #[must_use]
    pub fn with_v2(mut self, proxy: impl ConnectTo<Conductor>) -> Self {
        self.v2 = Some(DynConnectTo::new(proxy));
        self
    }
}

#[cfg(feature = "unstable_protocol_v2")]
impl ConnectTo<Conductor> for ProxyProtocolRouter {
    async fn connect_to(self, conductor: impl ConnectTo<Proxy>) -> Result<(), crate::Error> {
        let supported = SupportedProtocols {
            v1: self.v1.is_some(),
            v2: self.v2.is_some(),
        };
        let mut conductor = RunningProtocolPeer::new(conductor);
        let (first_frame, conductor, selected) = loop {
            let Some((mut frame, next_conductor)) = conductor.next_frame().await? else {
                return Ok(());
            };
            let message = match initialize_message_mut(&mut frame) {
                Ok(Some(message)) => message,
                Ok(None) => {
                    conductor = next_conductor;
                    continue;
                }
                Err(error) => return reject_initialize(next_conductor, &frame, error).await,
            };
            let selected = match select_proxy_protocol(message, supported) {
                Ok(selected) => selected,
                Err(error) => return reject_initialize(next_conductor, &frame, error).await,
            };
            break (frame, next_conductor, selected);
        };
        let Some(proxy) = selected.take_proxy(self) else {
            let error = selected.unsupported_error(supported);
            return reject_initialize(conductor, &first_frame, error).await;
        };

        let proxy = RunningProtocolPeer::new(proxy);
        proxy.send_frame(first_frame)?;
        pipe_protocol_peers_until_closed(conductor, proxy).await
    }
}

#[cfg(feature = "unstable_protocol_v2")]
impl SelectedProtocol {
    fn take_proxy(self, proxy: ProxyProtocolRouter) -> Option<DynConnectTo<Conductor>> {
        match self {
            Self::V1 => proxy.v1,
            Self::V2 => proxy.v2,
        }
    }
}

#[cfg(feature = "unstable_protocol_v2")]
fn select_proxy_protocol(
    message: &RawJsonRpcMessage,
    supported: SupportedProtocols,
) -> Result<SelectedProtocol, crate::Error> {
    let RawJsonRpcMessage::Request(request) = message else {
        return Err(crate::Error::invalid_request()
            .data("first ACP proxy message must be an `_proxy/initialize` request"));
    };

    if request.method.as_ref() != METHOD_INITIALIZE_PROXY {
        return Err(crate::Error::invalid_request()
            .data("first ACP proxy request must be `_proxy/initialize`"));
    }

    let Some(RawJsonRpcParams::Object(params)) = &request.params else {
        return Err(invalid_initialize_protocol_version());
    };
    let Some(protocol_version) = params.get("protocolVersion") else {
        return Err(invalid_initialize_protocol_version());
    };
    let requested = serde_json::from_value::<ProtocolVersion>(protocol_version.clone())
        .map_err(|_| invalid_initialize_protocol_version())?;
    let selected = supported.exact(requested).ok_or_else(|| {
        crate::Error::invalid_request().data(format!(
            "unsupported ACP protocol version {requested}; this proxy supports {}",
            supported.description()
        ))
    })?;

    match selected {
        SelectedProtocol::V1 => {
            parse_initialize_params::<crate::schema::InitializeProxyRequest>(params)?;
        }
        SelectedProtocol::V2 => {
            parse_initialize_params::<v2::InitializeProxyRequest>(params)?;
        }
    }
    Ok(selected)
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
        #[cfg(not(feature = "unstable_protocol_v2"))]
        {
            MatchDispatchFrom::new(message, &cx)
                .if_request_from(Client, async |_req: InitializeRequest, responder| {
                    responder.respond_with_error(crate::Error::invalid_request().data(format!(
                        "proxies must be initialized with `{METHOD_INITIALIZE_PROXY}`"
                    )))
                })
                .await
                .if_request_from(
                    Client,
                    async |request: InitializeProxyRequest, responder| {
                        let InitializeProxyRequest { initialize } = request;
                        cx.send_ordered_request_to(Agent, initialize)
                            .forward_response_to(responder)
                    },
                )
                .await
                .if_request_from(Client, async |request: NewSessionRequest, responder| {
                    let sent = cx.send_ordered_request_to(Agent, request);
                    let sent = sent.forward_cancellation_from(responder.cancellation());
                    sent.on_receiving_result({
                        let cx = cx.clone();
                        async move |result| {
                            if let Ok(NewSessionResponse { session_id, .. }) = &result {
                                cx.add_dynamic_handler(ProxySessionMessages::new(
                                    session_id.clone(),
                                ))?
                                .detach();
                            }
                            responder.respond_with_result(result)
                        }
                    })
                })
                .await
                .if_dispatch_from(Client, async |message: Dispatch| {
                    cx.send_proxied_message_to(Agent, message)
                })
                .await
                .if_dispatch_from(Agent, async |message: Dispatch| {
                    cx.send_proxied_message_to(Client, message)
                })
                .await
                .done()
        }

        #[cfg(feature = "unstable_protocol_v2")]
        {
            let message = match message {
                Dispatch::Request(request, responder) if request.method() == "initialize" => {
                    responder.respond_with_error(crate::Error::invalid_request().data(format!(
                        "proxies must be initialized with `{METHOD_INITIALIZE_PROXY}`"
                    )))?;
                    return Ok(Handled::Yes);
                }
                Dispatch::Request(mut request, responder)
                    if request.method() == METHOD_INITIALIZE_PROXY =>
                {
                    request.method = "initialize".to_string();
                    cx.send_ordered_request_to(Agent, request)
                        .forward_response_to(responder)?;
                    return Ok(Handled::Yes);
                }
                Dispatch::Request(request, responder) if request.method() == "session/new" => {
                    let sent = cx.send_ordered_request_to(Agent, request);
                    // The dynamic-handler hook below means we cannot use
                    // `forward_response_to`, so wire up cancellation forwarding
                    // explicitly to keep `session/new` cancellable like every
                    // other proxied request.
                    let sent = sent.forward_cancellation_from(responder.cancellation());
                    sent.on_receiving_result({
                        let cx = cx.clone();
                        async move |result| {
                            let result = result.and_then(|response| {
                                let envelope: NewSessionResponseEnvelope =
                                    crate::util::json_cast(response.clone())?;
                                cx.add_dynamic_handler(ProxySessionMessages::new(
                                    envelope.session_id,
                                ))?
                                .detach();
                                Ok(response)
                            });
                            responder.respond_with_result(result)
                        }
                    })?;
                    return Ok(Handled::Yes);
                }
                message => message,
            };

            MatchDispatchFrom::new(message, &cx)
                .if_dispatch_from(Client, async |message: Dispatch| {
                    cx.send_proxied_message_to(Agent, message)
                })
                .await
                .if_dispatch_from(Agent, async |message: Dispatch| {
                    cx.send_proxied_message_to(Client, message)
                })
                .await
                .done()
        }
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
            .if_dispatch_from(Agent, async |message| {
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
