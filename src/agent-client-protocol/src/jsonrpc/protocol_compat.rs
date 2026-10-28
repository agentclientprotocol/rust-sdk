/// The name and version substituted for a missing or malformed `info` when
/// an agent selects protocol v2 for a client that requested a newer version.
///
/// `info` is required in v2, but a newer client's identity may not parse
/// under v2. The request is accepted with this placeholder instead.
#[cfg(feature = "unstable_protocol_v2")]
pub(crate) const UNKNOWN_IMPLEMENTATION: &str = "unknown";

/// Canonicalize a newer client's `initialize` parameters as a v2 request.
///
/// The agent selected v2 for a request written for a newer version, so it
/// must ignore what it can't parse instead of rejecting the request. The
/// result is the v2 request that remains: each capability that does not parse
/// is dropped, `_meta` is dropped unless it is an object, fields v2 does not
/// define are dropped, and a missing or malformed `info` is replaced by
/// [`UNKNOWN_IMPLEMENTATION`] for both name and version. `protocolVersion` is
/// set to v2.
#[cfg(feature = "unstable_protocol_v2")]
pub(crate) fn normalize_newer_v2_initialize_request(
    params: &serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Map<String, serde_json::Value>, crate::Error> {
    use crate::schema::{ProtocolVersion, v2};
    use serde::Deserialize as _;

    let mut params = params.clone();
    let info_parses = params
        .get("info")
        .is_some_and(|info| v2::Implementation::deserialize(info).is_ok());
    if !info_parses {
        let unknown = v2::Implementation::new(UNKNOWN_IMPLEMENTATION, UNKNOWN_IMPLEMENTATION);
        params.insert(
            "info".into(),
            serde_json::to_value(unknown).map_err(crate::Error::into_internal_error)?,
        );
    }
    retain_parsable_capabilities::<v2::ClientCapabilities>(&mut params, "capabilities");
    retain_object(&mut params, "_meta");
    params.insert(
        "protocolVersion".into(),
        serde_json::to_value(ProtocolVersion::V2).map_err(crate::Error::into_internal_error)?,
    );
    let initialize: v2::InitializeRequest =
        serde_json::from_value(serde_json::Value::Object(params))?;
    let serde_json::Value::Object(normalized) =
        serde_json::to_value(initialize).map_err(crate::Error::into_internal_error)?
    else {
        return Err(crate::util::internal_error(
            "initialize params did not serialize to an object",
        ));
    };
    Ok(normalized)
}

/// Drop each capability under `key` that does not parse as part of `T`, or
/// the whole entry if it is not an object.
fn retain_parsable_capabilities<T: serde::de::DeserializeOwned>(
    params: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) {
    if !retain_object(params, key) {
        return;
    }
    if let Some(serde_json::Value::Object(capabilities)) = params.get_mut(key) {
        capabilities.retain(|name, value| {
            let single = serde_json::Map::from_iter([(name.clone(), value.clone())]);
            serde_json::from_value::<T>(serde_json::Value::Object(single)).is_ok()
        });
    }
}

/// Remove `key` unless it holds an object. Returns whether an object remains.
fn retain_object(params: &mut serde_json::Map<String, serde_json::Value>, key: &str) -> bool {
    match params.get(key) {
        Some(serde_json::Value::Object(_)) => true,
        Some(_) => {
            params.remove(key);
            false
        }
        None => false,
    }
}

/// Normalize a newer peer's initialization into the stable v1 surface.
///
/// Only shared capabilities are carried across. In particular, unknown newer
/// fields must never enable the v1 file-system or terminal-execution surfaces.
///
/// The request was written for a newer version, so parameters that don't
/// parse under v1 are dropped instead of rejecting it: a missing or malformed
/// `info` leaves v1's optional `clientInfo` unset, capabilities that don't
/// parse are dropped, and `_meta` is dropped unless it is an object.
pub(crate) fn normalize_v1_initialize_request(
    params: &serde_json::Value,
) -> Result<serde_json::Map<String, serde_json::Value>, crate::Error> {
    use crate::schema::{ProtocolVersion, v1};
    use serde::Deserialize as _;

    let info = params
        .get("info")
        .and_then(|info| v1::Implementation::deserialize(info).ok());
    let mut capabilities = serde_json::Map::new();
    if let Some(source) = params
        .get("capabilities")
        .and_then(serde_json::Value::as_object)
    {
        for key in ["auth", "elicitation", "nes", "positionEncodings", "_meta"] {
            if let Some(value) = source.get(key) {
                capabilities.insert(key.into(), value.clone());
            }
        }
    }
    if let Some(auth) = capabilities
        .get_mut("auth")
        .and_then(serde_json::Value::as_object_mut)
    {
        let terminal = auth
            .remove("terminal")
            .is_some_and(|value| value.is_object());
        auth.insert("terminal".into(), terminal.into());
    }
    capabilities.insert(
        "session".into(),
        serde_json::json!({ "configOptions": { "boolean": {} } }),
    );
    let mut target = serde_json::Map::from_iter([
        (
            "protocolVersion".to_owned(),
            serde_json::to_value(ProtocolVersion::V1).map_err(crate::Error::into_internal_error)?,
        ),
        ("clientCapabilities".to_owned(), capabilities.into()),
    ]);
    if let Some(info) = info {
        target.insert(
            "clientInfo".into(),
            serde_json::to_value(info).map_err(crate::Error::into_internal_error)?,
        );
    }
    if let Some(meta) = params.get("_meta") {
        target.insert("_meta".into(), meta.clone());
    }
    retain_parsable_capabilities::<v1::ClientCapabilities>(&mut target, "clientCapabilities");
    retain_object(&mut target, "_meta");
    let initialize: v1::InitializeRequest =
        serde_json::from_value(serde_json::Value::Object(target))?;
    let serde_json::Value::Object(normalized) =
        serde_json::to_value(initialize).map_err(crate::Error::into_internal_error)?
    else {
        return Err(crate::util::internal_error(
            "initialize params did not serialize to an object",
        ));
    };
    Ok(normalized)
}

/// The outcome of a v2 client's [`send_initialize`](crate::V2ConnectionTo::send_initialize).
///
/// A v1-only agent may legitimately answer a v2 initialization attempt with a
/// v1 response. That is a successful reply, not an error, so it is reported as
/// [`InitializeOutcome::LowerVersion`]. Errors from the agent, malformed
/// responses, and transport failures still resolve the request with an error.
#[cfg(feature = "unstable_protocol_v2")]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InitializeOutcome {
    /// The agent selected v2 and the connection is ready for v2 traffic.
    Initialized(crate::schema::v2::InitializeResponse),
    /// The agent selected a lower protocol version.
    ///
    /// The connection stays uninitialized: a v2 connection cannot carry the
    /// selected version's traffic, and the SDK never converts it. Fall back to
    /// an implementation of the selected version if that is acceptable.
    LowerVersion(InitializationVersionMismatch),
}

/// A successful `initialize` response that selected a lower protocol version
/// than the local API requires.
///
/// Reported by [`InitializeOutcome::LowerVersion`]. This is not the `-32022`
/// (`Unsupported protocol version`) error an agent returns when it supports no
/// version at or below the requested one.
#[cfg(feature = "unstable_protocol_v2")]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct InitializationVersionMismatch {
    /// The protocol version the local connection requested.
    pub requested: crate::schema::ProtocolVersion,
    /// The lower protocol version the peer selected.
    pub selected: crate::schema::ProtocolVersion,
    /// The peer's validated initialization response for the selected version.
    pub response: SelectedInitializeResponse,
}

#[cfg(feature = "unstable_protocol_v2")]
impl InitializationVersionMismatch {
    /// Describe a peer that answered a request for `requested` with
    /// `response`, selecting the response's protocol version.
    ///
    /// Useful for exercising fallback paths in tests.
    #[must_use]
    pub fn new(
        requested: crate::schema::ProtocolVersion,
        response: SelectedInitializeResponse,
    ) -> Self {
        Self {
            requested,
            selected: response.protocol_version(),
            response,
        }
    }
}

#[cfg(feature = "unstable_protocol_v2")]
impl std::fmt::Display for InitializationVersionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "requested ACP protocol version {} but peer selected {}",
            self.requested, self.selected,
        )
    }
}

/// A peer's initialization response, typed by the protocol version it selected.
///
/// New variants are added as later protocol versions make more versions
/// selectable below the one a client requests.
#[cfg(feature = "unstable_protocol_v2")]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SelectedInitializeResponse {
    /// The peer selected ACP protocol v1.
    V1(crate::schema::v1::InitializeResponse),
}

#[cfg(feature = "unstable_protocol_v2")]
impl SelectedInitializeResponse {
    /// The protocol version the response selects.
    #[must_use]
    pub fn protocol_version(&self) -> crate::schema::ProtocolVersion {
        match self {
            Self::V1(response) => response.protocol_version,
        }
    }

    /// Parse `value` as the initialization response for `version`, or return
    /// `None` if the SDK has no response type for that version.
    fn from_value(
        version: crate::schema::ProtocolVersion,
        value: serde_json::Value,
    ) -> Option<Result<Self, crate::Error>> {
        (version == crate::schema::ProtocolVersion::V1).then(|| {
            <crate::schema::v1::InitializeResponse as crate::JsonRpcResponse>::from_value(
                "initialize",
                value,
            )
            .map(Self::V1)
        })
    }
}

#[cfg(feature = "unstable_protocol_v2")]
impl crate::JsonRpcResponse for InitializeOutcome {
    fn into_json(self, method: &str) -> Result<serde_json::Value, crate::Error> {
        match self {
            Self::Initialized(response) => response.into_json(method),
            Self::LowerVersion(mismatch) => match mismatch.response {
                SelectedInitializeResponse::V1(response) => response.into_json(method),
            },
        }
    }

    fn from_value(method: &str, value: serde_json::Value) -> Result<Self, crate::Error> {
        let selected: crate::schema::ProtocolVersion = value
            .get("protocolVersion")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .ok()
            .flatten()
            .ok_or_else(|| {
                crate::Error::invalid_params()
                    .data("initialize.protocolVersion must be a valid ACP protocol version")
            })?;
        let requested = crate::schema::ProtocolVersion::V2;
        if selected == requested {
            return crate::schema::v2::InitializeResponse::from_value(method, value)
                .map(Self::Initialized);
        }
        match SelectedInitializeResponse::from_value(selected, value) {
            Some(response) if selected < requested => Ok(Self::LowerVersion(
                InitializationVersionMismatch::new(requested, response?),
            )),
            _ => Err(crate::Error::invalid_request().data(format!(
                "unsupported ACP protocol version {selected} in initialize response",
            ))),
        }
    }
}

/// The request sent by [`send_initialize`](crate::V2ConnectionTo::send_initialize).
///
/// Its pending reply opts into receiving a lower-version response as a
/// successful [`InitializeOutcome`] instead of the connection's local error.
#[cfg(feature = "unstable_protocol_v2")]
#[derive(Debug, Clone)]
pub(crate) struct InitializeNegotiation(pub(crate) crate::schema::v2::InitializeRequest);

#[cfg(feature = "unstable_protocol_v2")]
impl crate::JsonRpcMessage for InitializeNegotiation {
    fn matches_method(method: &str) -> bool {
        crate::schema::v2::InitializeRequest::matches_method(method)
    }

    fn method(&self) -> &str {
        self.0.method()
    }

    fn to_untyped_message(&self) -> Result<crate::UntypedMessage, crate::Error> {
        self.0.to_untyped_message()
    }

    fn parse_message(method: &str, params: &impl serde::Serialize) -> Result<Self, crate::Error> {
        crate::schema::v2::InitializeRequest::parse_message(method, params).map(Self)
    }
}

#[cfg(feature = "unstable_protocol_v2")]
impl crate::JsonRpcRequest for InitializeNegotiation {
    type Response = InitializeOutcome;
}

mod imp {
    use std::sync::{Arc, Mutex};

    use crate::schema::{ProtocolVersion, v1::RequestId};
    use crate::{UntypedMessage, role::RemoteStyle};

    #[derive(Clone, Copy, Debug)]
    pub(crate) enum ProtocolMode {
        Disabled,
        Acp(AcpProtocolMode),
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct AcpProtocolMode {
        api: ProtocolVersionKind,
        initialize_surface: InitializeSurface,
        initialization_role: InitializationRole,
        /// Reject incoming requests other than `initialize` until an
        /// initialization has completed, without the rest of the peer
        /// lifecycle. V1 agents with an initialize handler set this; v2
        /// responders get it from [`InitializationRole::Responder`].
        requires_initialize: bool,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum InitializeSurface {
        Peer,
        Proxy,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[cfg_attr(not(feature = "unstable_protocol_v2"), allow(dead_code))]
    enum InitializationRole {
        /// Preserve the v1 and proxy initialization behavior. A v2 proxy has
        /// both an incoming predecessor initialization and an outgoing
        /// successor initialization, so the peer lifecycle does not apply to
        /// it.
        Unchecked,
        Initiator,
        Responder,
    }

    impl AcpProtocolMode {
        fn is_incoming_initialize_request(self, method: &str) -> bool {
            match self.initialize_surface {
                InitializeSurface::Peer => method == "initialize",
                InitializeSurface::Proxy => method == "_proxy/initialize",
            }
        }

        fn is_incoming_initialize_response(method: &str) -> bool {
            // Pending replies retain the logical method from before successor
            // wrapping, so a proxy's downstream initialize response is also
            // keyed by `initialize`.
            method == "initialize"
        }

        fn is_outgoing_initialize_response(self, method: &str) -> bool {
            match self.initialize_surface {
                InitializeSurface::Peer => method == "initialize",
                InitializeSurface::Proxy => method == "_proxy/initialize",
            }
        }
    }

    impl ProtocolMode {
        pub(crate) fn disabled() -> Self {
            Self::Disabled
        }

        pub(crate) fn v1_agent() -> Self {
            Self::Acp(AcpProtocolMode {
                api: ProtocolVersionKind::V1,
                initialize_surface: InitializeSurface::Peer,
                initialization_role: InitializationRole::Unchecked,
                requires_initialize: false,
            })
        }

        /// Reject incoming requests other than `initialize` until an
        /// initialization completes. Only applies to v1 peer modes; v2
        /// responders already enforce the full lifecycle.
        pub(crate) fn require_initialize(self) -> Self {
            match self {
                Self::Acp(mode)
                    if mode.api == ProtocolVersionKind::V1
                        && mode.initialize_surface == InitializeSurface::Peer =>
                {
                    Self::Acp(AcpProtocolMode {
                        requires_initialize: true,
                        ..mode
                    })
                }
                mode => mode,
            }
        }

        pub(crate) fn v1_client() -> Self {
            Self::Acp(AcpProtocolMode {
                api: ProtocolVersionKind::V1,
                initialize_surface: InitializeSurface::Peer,
                initialization_role: InitializationRole::Unchecked,
                requires_initialize: false,
            })
        }

        pub(crate) fn v1_proxy() -> Self {
            Self::Acp(AcpProtocolMode {
                api: ProtocolVersionKind::V1,
                initialize_surface: InitializeSurface::Proxy,
                initialization_role: InitializationRole::Unchecked,
                requires_initialize: false,
            })
        }

        #[cfg(feature = "unstable_protocol_v2")]
        pub(crate) fn v2_agent() -> Self {
            Self::Acp(AcpProtocolMode {
                api: ProtocolVersionKind::V2,
                initialize_surface: InitializeSurface::Peer,
                initialization_role: InitializationRole::Responder,
                requires_initialize: false,
            })
        }

        #[cfg(feature = "unstable_protocol_v2")]
        pub(crate) fn v2_client() -> Self {
            Self::Acp(AcpProtocolMode {
                api: ProtocolVersionKind::V2,
                initialize_surface: InitializeSurface::Peer,
                initialization_role: InitializationRole::Initiator,
                requires_initialize: false,
            })
        }

        #[cfg(feature = "unstable_protocol_v2")]
        pub(crate) fn v2_proxy() -> Self {
            Self::Acp(AcpProtocolMode {
                api: ProtocolVersionKind::V2,
                initialize_surface: InitializeSurface::Proxy,
                initialization_role: InitializationRole::Unchecked,
                requires_initialize: false,
            })
        }

        pub(crate) fn merge(self, other: Self) -> Self {
            match (self, other) {
                (Self::Disabled, other) => other,
                (this, Self::Disabled) => this,
                (Self::Acp(this), Self::Acp(other)) => {
                    assert_eq!(
                        this.api, other.api,
                        "cannot merge ACP builders with different API protocol versions; \
                         handler chains share a single API surface",
                    );
                    assert_eq!(
                        this.initialize_surface, other.initialize_surface,
                        "cannot merge standard ACP and proxy ACP builders; \
                         handler chains share one initialization surface",
                    );
                    assert_eq!(
                        this.initialization_role, other.initialization_role,
                        "cannot merge ACP builders with different initialization roles; \
                         handler chains share one connection lifecycle",
                    );
                    Self::Acp(AcpProtocolMode {
                        requires_initialize: this.requires_initialize || other.requires_initialize,
                        ..this
                    })
                }
            }
        }

        #[cfg(feature = "unstable_protocol_v2")]
        pub(crate) fn api_protocol_version(self) -> Option<ProtocolVersion> {
            match self {
                Self::Disabled => None,
                Self::Acp(mode) => Some(mode.api.as_protocol_version()),
            }
        }
    }

    #[derive(Clone, Debug)]
    pub(crate) struct ProtocolCompat {
        mode: Option<AcpProtocolMode>,
        state: Arc<Mutex<ProtocolState>>,
    }

    #[derive(Debug)]
    struct ProtocolState {
        negotiated: ProtocolVersionKind,
        pending_initialize: Option<ProtocolVersionKind>,
        incoming_initialize_id: Option<RequestId>,
        initialization: InitializationState,
        /// Whether any initialization has completed on this connection.
        initialized: bool,
        /// Whether a completed handshake may still be repeated.
        ///
        /// A client may repeat `initialize` after the previous one completed
        /// and before it sends any other request; the latest one is
        /// authoritative. The first request the client sends closes this
        /// window for the rest of the connection.
        repeatable: bool,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum InitializationState {
        Unchecked,
        Uninitialized,
        Initializing,
        Ready,
    }

    /// How an initialize response that passed validation completes the
    /// handshake.
    enum InitializeResponseCheck {
        Negotiated(ProtocolVersionKind),
        /// A validated lower selection for a `send_initialize` request.
        #[cfg_attr(not(feature = "unstable_protocol_v2"), allow(dead_code))]
        LowerVersion,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum ProtocolVersionKind {
        V1,
        V2,
    }

    impl ProtocolVersionKind {
        fn as_protocol_version(self) -> ProtocolVersion {
            match self {
                Self::V1 => ProtocolVersion::V1,
                Self::V2 => ProtocolVersion::from(2_u16),
            }
        }

        fn from_protocol_version(version: ProtocolVersion) -> Option<Self> {
            if version == ProtocolVersion::V1 {
                Some(Self::V1)
            } else if version == ProtocolVersion::from(2_u16) {
                Some(Self::V2)
            } else {
                None
            }
        }
    }

    impl ProtocolCompat {
        pub(crate) fn new(mode: ProtocolMode) -> Self {
            let mode = match mode {
                ProtocolMode::Disabled => None,
                ProtocolMode::Acp(mode) => Some(mode),
            };
            let negotiated = mode.map_or(ProtocolVersionKind::V1, |mode| mode.api);
            let initialization = match mode.map(|mode| mode.initialization_role) {
                Some(InitializationRole::Initiator | InitializationRole::Responder) => {
                    InitializationState::Uninitialized
                }
                Some(InitializationRole::Unchecked) | None => InitializationState::Unchecked,
            };

            Self {
                mode,
                state: Arc::new(Mutex::new(ProtocolState {
                    negotiated,
                    pending_initialize: None,
                    incoming_initialize_id: None,
                    initialization,
                    initialized: false,
                    repeatable: false,
                })),
            }
        }

        #[cfg(all(test, feature = "unstable_protocol_v2"))]
        pub(crate) fn incoming_message(
            &self,
            message: UntypedMessage,
        ) -> Result<UntypedMessage, crate::Error> {
            self.incoming_request(&RequestId::Number(0), message)
        }

        pub(crate) fn incoming_request(
            &self,
            id: &RequestId,
            message: UntypedMessage,
        ) -> Result<UntypedMessage, crate::Error> {
            let Some(mode) = self.mode else {
                return Ok(message);
            };

            // V2 request ids must be strings or integers. The rejection is
            // still answered with `id: null`, as for any request whose id
            // cannot be determined.
            if mode.api == ProtocolVersionKind::V2 && *id == RequestId::Null {
                return Err(null_request_id());
            }

            if mode.is_incoming_initialize_request(message.method()) {
                return self.incoming_initialize_request(mode, id, message);
            }
            if mode.initialize_surface == InitializeSurface::Proxy
                && (message.method() == "initialize" || successor_encloses_initialize(&message))
            {
                return Err(invalid_proxy_initialize_direction());
            }

            if mode.requires_initialize && !self.has_initialized() {
                return Err(initialization_required(message.method()));
            }
            self.ensure_initialized(message.method())?;
            ensure_matching_protocol_version(
                message.method(),
                self.active_wire_version(),
                mode.api,
            )?;
            if mode.initialization_role == InitializationRole::Responder {
                self.close_repeat_window();
            }
            Ok(message)
        }

        pub(crate) fn outgoing_message(
            &self,
            mut message: UntypedMessage,
            remote_style: RemoteStyle,
        ) -> Result<UntypedMessage, crate::Error> {
            let Some(mode) = self.mode else {
                return Ok(message);
            };

            let wire_version = if let Some(params) =
                outgoing_initialize_params(mode, remote_style, &mut message)?
            {
                set_protocol_version(params, mode.api)?;
                validate_native_v2_initialize_request(mode, params)?;
                self.begin_outgoing_initialize(mode)?;
                mode.api
            } else {
                self.ensure_initialized(message.method())?;
                self.active_wire_version()
            };

            ensure_matching_protocol_version(message.method(), mode.api, wire_version)?;
            if mode.initialization_role == InitializationRole::Initiator {
                self.close_repeat_window();
            }
            Ok(message)
        }

        pub(crate) fn incoming_notification(
            &self,
            message: UntypedMessage,
        ) -> Result<Vec<UntypedMessage>, crate::Error> {
            let Some(mode) = self.mode else {
                return Ok(vec![message]);
            };

            self.ensure_notification_allowed(message.method())?;
            ensure_matching_protocol_version(
                message.method(),
                self.active_wire_version(),
                mode.api,
            )?;
            Ok(vec![message])
        }

        pub(crate) fn outgoing_notification(
            &self,
            message: UntypedMessage,
        ) -> Result<Vec<UntypedMessage>, crate::Error> {
            let Some(mode) = self.mode else {
                return Ok(vec![message]);
            };

            self.ensure_notification_allowed(message.method())?;
            ensure_matching_protocol_version(
                message.method(),
                mode.api,
                self.active_wire_version(),
            )?;
            Ok(vec![message])
        }

        /// Check an incoming response against the protocol state.
        ///
        /// `accepts_lower_version` marks a pending
        /// [`super::InitializeNegotiation`], which receives a validated
        /// lower-version initialize response as a success.
        pub(crate) fn incoming_response(
            &self,
            method: &str,
            accepts_lower_version: bool,
            result: Result<serde_json::Value, crate::Error>,
        ) -> Result<serde_json::Value, crate::Error> {
            let Some(mode) = self.mode else {
                return result;
            };

            if AcpProtocolMode::is_incoming_initialize_response(method) {
                return self.incoming_initialize_response(mode, accepts_lower_version, result);
            }

            let value = result?;
            self.ensure_initialized(method)?;
            ensure_matching_protocol_version(method, self.active_wire_version(), mode.api)?;
            Ok(value)
        }

        #[cfg(all(test, feature = "unstable_protocol_v2"))]
        pub(crate) fn outgoing_response(
            &self,
            method: &str,
            result: Result<serde_json::Value, crate::Error>,
        ) -> Result<serde_json::Value, crate::Error> {
            self.outgoing_response_to(&RequestId::Number(0), method, result)
        }

        pub(crate) fn outgoing_response_to(
            &self,
            id: &RequestId,
            method: &str,
            result: Result<serde_json::Value, crate::Error>,
        ) -> Result<serde_json::Value, crate::Error> {
            let Some(mode) = self.mode else {
                return result;
            };

            if mode.is_outgoing_initialize_response(method) {
                match mode.initialization_role {
                    InitializationRole::Initiator => {
                        return result.and_then(|_| {
                            Err(unexpected_initialize_response(mode.initialization_role))
                        });
                    }
                    InitializationRole::Responder if !self.is_pending_incoming_initialize(id) => {
                        return result.and_then(|_| {
                            Err(unexpected_initialize_response(mode.initialization_role))
                        });
                    }
                    InitializationRole::Unchecked | InitializationRole::Responder => {}
                }
                let mut value = match result {
                    Ok(value) => value,
                    Err(error) => {
                        self.fail_initialize();
                        return Err(error);
                    }
                };
                let negotiated = self.pending_initialize().or_else(|| {
                    (mode.initialization_role == InitializationRole::Unchecked).then_some(mode.api)
                });
                let negotiated = negotiated
                    .ok_or_else(|| unexpected_initialize_response(mode.initialization_role))?;
                if let Err(error) = ensure_matching_protocol_version(method, mode.api, negotiated)
                    .and_then(|()| set_protocol_version(&mut value, negotiated))
                    .and_then(|()| validate_native_v2_initialize_response(mode, &value))
                {
                    self.fail_initialize();
                    return Err(error);
                }
                self.complete_initialize(negotiated)?;
                return Ok(value);
            }

            let value = result?;
            self.ensure_initialized(method)?;
            let wire_version = self.active_wire_version();

            ensure_matching_protocol_version(method, mode.api, wire_version)?;
            Ok(value)
        }

        fn incoming_initialize_request(
            &self,
            mode: AcpProtocolMode,
            id: &RequestId,
            mut message: UntypedMessage,
        ) -> Result<UntypedMessage, crate::Error> {
            let requested = required_protocol_version_from_value(message.params())?;
            if mode.initialization_role == InitializationRole::Responder {
                self.ensure_repeat_keeps_version(requested)?;
            }
            if mode.initialize_surface == InitializeSurface::Peer {
                // V1 retains its original fallback-to-latest bootstrap. Only
                // v2 endpoints reject a version below their supported range.
                if mode.api == ProtocolVersionKind::V2 && requested < mode.api.as_protocol_version()
                {
                    return Err(crate::util::unsupported_protocol_version(
                        requested,
                        &[mode.api.as_protocol_version()],
                    ));
                }
                if mode.api == ProtocolVersionKind::V1 && requested > ProtocolVersion::V1 {
                    message.params = serde_json::Value::Object(
                        super::normalize_v1_initialize_request(message.params())?,
                    );
                }
                // A request for a newer version keeps what parses under v2.
                // An exact v2 request is validated strictly below.
                #[cfg(feature = "unstable_protocol_v2")]
                if mode.api == ProtocolVersionKind::V2 && requested > mode.api.as_protocol_version()
                {
                    let serde_json::Value::Object(params) = message.params() else {
                        return Err(invalid_initialize_protocol_version());
                    };
                    message.params = serde_json::Value::Object(
                        super::normalize_newer_v2_initialize_request(params)?,
                    );
                }
            } else if ProtocolVersionKind::from_protocol_version(requested) != Some(mode.api) {
                // The conductor has already selected a proxy's wire version.
                return Err(unsupported_protocol_version(requested, mode.api));
            }

            set_protocol_version(&mut message.params, mode.api)?;
            validate_native_v2_initialize_request(mode, message.params())?;
            self.begin_incoming_initialize(mode, id)?;
            Ok(message)
        }

        fn incoming_initialize_response(
            &self,
            mode: AcpProtocolMode,
            accepts_lower_version: bool,
            result: Result<serde_json::Value, crate::Error>,
        ) -> Result<serde_json::Value, crate::Error> {
            let mut value = match result {
                Ok(value) => value,
                Err(error) => {
                    self.fail_initialize();
                    return Err(error);
                }
            };
            let response = (|| {
                let pending = self.pending_initialize().or_else(|| {
                    (mode.initialization_role == InitializationRole::Unchecked).then_some(mode.api)
                });
                let pending = pending
                    .ok_or_else(|| unexpected_initialize_response(mode.initialization_role))?;
                let response_version = required_protocol_version_from_value(&value)?;
                let wire_version = ProtocolVersionKind::from_protocol_version(response_version)
                    .ok_or_else(|| unsupported_protocol_version(response_version, mode.api))?;
                // Only a client's own handshake, sent by `send_initialize`,
                // has a caller that can fall back. A proxy relays its
                // successor's outcome, and the conductor has already fixed
                // that wire version.
                #[cfg(feature = "unstable_protocol_v2")]
                if accepts_lower_version
                    && mode.initialization_role == InitializationRole::Initiator
                    && wire_version < mode.api
                    && let Some(response) = super::SelectedInitializeResponse::from_value(
                        wire_version.as_protocol_version(),
                        value.clone(),
                    )
                {
                    response?;
                    return Ok(InitializeResponseCheck::LowerVersion);
                }
                #[cfg(not(feature = "unstable_protocol_v2"))]
                let _ = accepts_lower_version;
                if wire_version != mode.api {
                    return Err(required_protocol_version(mode.api, wire_version));
                }
                ensure_matching_protocol_version("initialize", pending, wire_version)?;
                set_protocol_version(&mut value, wire_version)?;
                validate_native_v2_initialize_response(mode, &value)?;
                Ok(InitializeResponseCheck::Negotiated(wire_version))
            })();

            match response {
                Ok(InitializeResponseCheck::Negotiated(wire_version)) => {
                    self.complete_initialize(wire_version)?;
                    Ok(value)
                }
                // The caller sees the response, but this connection cannot
                // use it.
                Ok(InitializeResponseCheck::LowerVersion) => {
                    self.fail_initialize();
                    Ok(value)
                }
                Err(error) => {
                    self.fail_initialize();
                    Err(error)
                }
            }
        }

        fn begin_incoming_initialize(
            &self,
            mode: AcpProtocolMode,
            id: &RequestId,
        ) -> Result<(), crate::Error> {
            match mode.initialization_role {
                InitializationRole::Initiator => return Err(invalid_initialize_direction()),
                InitializationRole::Unchecked | InitializationRole::Responder => {}
            }
            self.begin_initialize(mode.api, Some(id))
        }

        fn begin_outgoing_initialize(&self, mode: AcpProtocolMode) -> Result<(), crate::Error> {
            match mode.initialization_role {
                InitializationRole::Responder => return Err(invalid_initialize_direction()),
                InitializationRole::Unchecked | InitializationRole::Initiator => {}
            }
            self.begin_initialize(mode.api, None)
        }

        fn begin_initialize(
            &self,
            requested: ProtocolVersionKind,
            incoming_id: Option<&RequestId>,
        ) -> Result<(), crate::Error> {
            let mut state = self
                .state
                .lock()
                .expect("protocol compatibility state mutex poisoned");
            match state.initialization {
                InitializationState::Unchecked => {
                    state.pending_initialize = Some(requested);
                    Ok(())
                }
                InitializationState::Uninitialized => {
                    state.initialization = InitializationState::Initializing;
                    state.pending_initialize = Some(requested);
                    state.incoming_initialize_id = incoming_id.cloned();
                    Ok(())
                }
                InitializationState::Initializing => Err(crate::Error::invalid_request()
                    .data("ACP initialization is already in progress on this connection")),
                // The client may repeat a completed handshake before its first
                // other request. The repeat replaces the previous one.
                InitializationState::Ready if state.repeatable => {
                    state.initialization = InitializationState::Initializing;
                    state.repeatable = false;
                    state.pending_initialize = Some(requested);
                    state.incoming_initialize_id = incoming_id.cloned();
                    Ok(())
                }
                InitializationState::Ready => Err(crate::Error::invalid_request().data(
                    "ACP initialization cannot be repeated after any other request; reconnect to initialize again",
                )),
            }
        }

        /// Record that the client sent a request other than `initialize`.
        fn close_repeat_window(&self) {
            self.state
                .lock()
                .expect("protocol compatibility state mutex poisoned")
                .repeatable = false;
        }

        fn ensure_initialized(&self, method: &str) -> Result<(), crate::Error> {
            let state = self
                .state
                .lock()
                .expect("protocol compatibility state mutex poisoned")
                .initialization;
            match state {
                InitializationState::Unchecked | InitializationState::Ready => Ok(()),
                InitializationState::Uninitialized | InitializationState::Initializing => {
                    Err(initialization_required(method))
                }
            }
        }

        fn ensure_notification_allowed(&self, method: &str) -> Result<(), crate::Error> {
            let initialization = self
                .state
                .lock()
                .expect("protocol compatibility state mutex poisoned")
                .initialization;
            if initialization == InitializationState::Initializing && method == "$/cancel_request" {
                return Ok(());
            }
            self.ensure_initialized(method)
        }

        fn complete_initialize(&self, negotiated: ProtocolVersionKind) -> Result<(), crate::Error> {
            let mut state = self
                .state
                .lock()
                .expect("protocol compatibility state mutex poisoned");
            if state.pending_initialize.is_none()
                && state.initialization != InitializationState::Unchecked
            {
                return Err(unexpected_initialize_response(
                    self.mode
                        .expect("protocol initialization requires an ACP mode")
                        .initialization_role,
                ));
            }
            if !matches!(
                state.initialization,
                InitializationState::Unchecked | InitializationState::Initializing
            ) {
                return Err(unexpected_initialize_response(
                    self.mode
                        .expect("protocol initialization requires an ACP mode")
                        .initialization_role,
                ));
            }
            state.pending_initialize = None;
            state.incoming_initialize_id = None;
            state.negotiated = negotiated;
            state.initialized = true;
            if state.initialization == InitializationState::Initializing {
                state.initialization = InitializationState::Ready;
                state.repeatable = true;
            }
            Ok(())
        }

        fn fail_initialize(&self) {
            let mut state = self
                .state
                .lock()
                .expect("protocol compatibility state mutex poisoned");
            state.pending_initialize = None;
            state.incoming_initialize_id = None;
            if state.initialization == InitializationState::Initializing {
                // A failed repeat keeps the previous initialization in place,
                // and the client may repeat again.
                if state.initialized {
                    state.initialization = InitializationState::Ready;
                    state.repeatable = true;
                } else {
                    state.initialization = InitializationState::Uninitialized;
                }
            }
        }

        /// Reject a repeated initialize requesting a version below the one
        /// the previous initialization selected. The agent never re-selects.
        fn ensure_repeat_keeps_version(
            &self,
            requested: ProtocolVersion,
        ) -> Result<(), crate::Error> {
            let state = self
                .state
                .lock()
                .expect("protocol compatibility state mutex poisoned");
            let selected = state.negotiated.as_protocol_version();
            if state.initialized && requested < selected {
                return Err(crate::Error::invalid_request().data(format!(
                    "a repeated initialize must not request a version below the selected ACP protocol version {selected}",
                )));
            }
            Ok(())
        }

        fn has_initialized(&self) -> bool {
            self.state
                .lock()
                .expect("protocol compatibility state mutex poisoned")
                .initialized
        }

        fn is_pending_incoming_initialize(&self, id: &RequestId) -> bool {
            self.state
                .lock()
                .expect("protocol compatibility state mutex poisoned")
                .incoming_initialize_id
                .as_ref()
                == Some(id)
        }

        fn active_wire_version(&self) -> ProtocolVersionKind {
            let state = self
                .state
                .lock()
                .expect("protocol compatibility state mutex poisoned");
            state.pending_initialize.unwrap_or(state.negotiated)
        }

        fn pending_initialize(&self) -> Option<ProtocolVersionKind> {
            self.state
                .lock()
                .expect("protocol compatibility state mutex poisoned")
                .pending_initialize
        }
    }

    fn required_protocol_version_from_value(
        value: &serde_json::Value,
    ) -> Result<ProtocolVersion, crate::Error> {
        let Some(version) = value.get("protocolVersion") else {
            return Err(invalid_initialize_protocol_version());
        };

        serde_json::from_value(version.clone()).map_err(|_| invalid_initialize_protocol_version())
    }

    fn outgoing_initialize_params(
        mode: AcpProtocolMode,
        remote_style: RemoteStyle,
        message: &mut UntypedMessage,
    ) -> Result<Option<&mut serde_json::Value>, crate::Error> {
        if successor_encloses_initialize(message) {
            return Err(invalid_prewrapped_initialize());
        }

        match mode.initialize_surface {
            InitializeSurface::Peer => {
                Ok((message.method() == "initialize").then_some(&mut message.params))
            }
            InitializeSurface::Proxy => {
                if message.method() == "initialize" {
                    if remote_style != RemoteStyle::Successor {
                        return Err(invalid_proxy_initialize_direction());
                    }
                    return Ok(Some(&mut message.params));
                }
                if message.method() == "_proxy/initialize" {
                    return Err(invalid_proxy_initialize_direction());
                }
                Ok(None)
            }
        }
    }

    fn successor_encloses_initialize(message: &UntypedMessage) -> bool {
        let mut method = message.method();
        let mut params = message.params();
        let mut wrapped = false;

        while method == "_proxy/successor" {
            wrapped = true;
            let Some(inner_method) = params.get("method").and_then(serde_json::Value::as_str)
            else {
                return false;
            };
            method = inner_method;
            if method == "_proxy/successor" {
                let Some(inner_params) = params.get("params") else {
                    return false;
                };
                params = inner_params;
            }
        }

        wrapped && matches!(method, "initialize" | "_proxy/initialize")
    }

    fn invalid_initialize_protocol_version() -> crate::Error {
        crate::Error::invalid_params()
            .data("initialize.protocolVersion must be a valid ACP protocol version")
    }

    fn initialization_required(method: &str) -> crate::Error {
        crate::Error::invalid_request().data(format!(
            "ACP initialization must complete before `{method}` can be used",
        ))
    }

    fn null_request_id() -> crate::Error {
        crate::Error::invalid_request()
            .data("ACP protocol v2 request ids must be a string or an integer, not null")
    }

    fn invalid_initialize_direction() -> crate::Error {
        crate::Error::invalid_request()
            .data("ACP clients send `initialize` requests and ACP agents respond to them")
    }

    fn unexpected_initialize_response(role: InitializationRole) -> crate::Error {
        let detail = match role {
            InitializationRole::Initiator => "before an initialize request is pending",
            InitializationRole::Responder => "before an initialize request was received",
            InitializationRole::Unchecked => "without a pending initialize request",
        };
        crate::Error::invalid_request().data(format!("received an initialize response {detail}"))
    }

    fn invalid_proxy_initialize_direction() -> crate::Error {
        crate::Error::invalid_request().data(
            "proxy initialization must arrive as `_proxy/initialize`; outgoing `initialize` must target the successor so the connection can apply `_proxy/successor`",
        )
    }

    fn invalid_prewrapped_initialize() -> crate::Error {
        crate::Error::invalid_request().data(
            "initialize requests must be sent as a logical `initialize` message; `_proxy/successor` wrapping is applied by connection routing",
        )
    }

    fn set_protocol_version(
        value: &mut serde_json::Value,
        version: ProtocolVersionKind,
    ) -> Result<(), crate::Error> {
        let serde_json::Value::Object(object) = value else {
            return Err(invalid_initialize_protocol_version());
        };
        object.insert(
            "protocolVersion".into(),
            serde_json::to_value(version.as_protocol_version())
                .map_err(crate::Error::into_internal_error)?,
        );
        Ok(())
    }

    // The shared guard stays fallible when v2 validation is compiled out.
    #[cfg_attr(
        not(feature = "unstable_protocol_v2"),
        allow(clippy::unnecessary_wraps)
    )]
    fn validate_native_v2_initialize_request(
        mode: AcpProtocolMode,
        value: &serde_json::Value,
    ) -> Result<(), crate::Error> {
        #[cfg(feature = "unstable_protocol_v2")]
        if mode.initialization_role != InitializationRole::Unchecked {
            <crate::schema::v2::InitializeRequest as crate::JsonRpcMessage>::parse_message(
                "initialize",
                value,
            )?;
        }
        #[cfg(not(feature = "unstable_protocol_v2"))]
        let _ = (mode, value);
        Ok(())
    }

    #[cfg_attr(
        not(feature = "unstable_protocol_v2"),
        allow(clippy::unnecessary_wraps)
    )]
    fn validate_native_v2_initialize_response(
        mode: AcpProtocolMode,
        value: &serde_json::Value,
    ) -> Result<(), crate::Error> {
        #[cfg(feature = "unstable_protocol_v2")]
        if mode.initialization_role != InitializationRole::Unchecked {
            <crate::schema::v2::InitializeResponse as crate::JsonRpcResponse>::from_value(
                "initialize",
                value.clone(),
            )?;
        }
        #[cfg(not(feature = "unstable_protocol_v2"))]
        let _ = (mode, value);
        Ok(())
    }

    fn ensure_matching_protocol_version(
        method: &str,
        from: ProtocolVersionKind,
        to: ProtocolVersionKind,
    ) -> Result<(), crate::Error> {
        if from == to {
            return Ok(());
        }

        Err(crate::Error::invalid_request().data(format!(
            "ACP protocol translation from {} to {} is not supported for `{method}`; register a handler for the negotiated protocol version",
            from.as_protocol_version(),
            to.as_protocol_version(),
        )))
    }

    fn unsupported_protocol_version(
        version: ProtocolVersion,
        supported: ProtocolVersionKind,
    ) -> crate::Error {
        crate::Error::invalid_request().data(format!(
            "unsupported ACP protocol version {version}; this endpoint only supports ACP protocol version {}",
            supported.as_protocol_version(),
        ))
    }

    fn required_protocol_version(
        required: ProtocolVersionKind,
        negotiated: ProtocolVersionKind,
    ) -> crate::Error {
        crate::Error::invalid_request().data(format!(
            "required ACP protocol version {} but peer negotiated {}; use a matching implementation for the negotiated protocol version",
            required.as_protocol_version(),
            negotiated.as_protocol_version(),
        ))
    }

    #[cfg(all(test, feature = "unstable_protocol_v2"))]
    mod tests {
        use super::*;
        use agent_client_protocol_schema::v2;

        fn negotiated(compat: &ProtocolCompat) -> ProtocolVersionKind {
            compat
                .state
                .lock()
                .expect("protocol compatibility state mutex poisoned")
                .negotiated
        }

        fn pending_initialize(compat: &ProtocolCompat) -> Option<ProtocolVersionKind> {
            compat
                .state
                .lock()
                .expect("protocol compatibility state mutex poisoned")
                .pending_initialize
        }

        fn initialization_state(compat: &ProtocolCompat) -> InitializationState {
            compat
                .state
                .lock()
                .expect("protocol compatibility state mutex poisoned")
                .initialization
        }

        fn v2_implementation() -> v2::Implementation {
            v2::Implementation::new("protocol-compat-test", env!("CARGO_PKG_VERSION"))
        }

        fn v2_initialize_request(protocol_version: ProtocolVersion) -> v2::InitializeRequest {
            v2::InitializeRequest::new(protocol_version, v2_implementation())
        }

        fn v2_initialize_response(protocol_version: ProtocolVersion) -> v2::InitializeResponse {
            v2::InitializeResponse::new(protocol_version, v2_implementation())
        }

        #[test]
        fn initialize_request_sets_active_wire_version_before_response() -> Result<(), crate::Error>
        {
            let compat = ProtocolCompat::new(ProtocolMode::v2_agent());
            assert_eq!(compat.active_wire_version(), ProtocolVersionKind::V2);
            assert_eq!(
                initialization_state(&compat),
                InitializationState::Uninitialized
            );

            compat.incoming_message(UntypedMessage::new(
                "initialize",
                v2_initialize_request(ProtocolVersion::V2),
            )?)?;

            assert_eq!(negotiated(&compat), ProtocolVersionKind::V2);
            assert_eq!(compat.active_wire_version(), ProtocolVersionKind::V2);
            assert_eq!(
                initialization_state(&compat),
                InitializationState::Initializing
            );

            compat.outgoing_response(
                "initialize",
                Ok(serde_json::to_value(v2_initialize_response(
                    ProtocolVersion::V2,
                ))?),
            )?;

            assert_eq!(negotiated(&compat), ProtocolVersionKind::V2);
            assert_eq!(compat.active_wire_version(), ProtocolVersionKind::V2);
            assert_eq!(initialization_state(&compat), InitializationState::Ready);
            Ok(())
        }

        #[test]
        fn outgoing_initialize_sets_active_wire_version_before_response() -> Result<(), crate::Error>
        {
            let compat = ProtocolCompat::new(ProtocolMode::v2_client());
            assert_eq!(compat.active_wire_version(), ProtocolVersionKind::V2);
            assert_eq!(
                initialization_state(&compat),
                InitializationState::Uninitialized
            );

            compat.outgoing_message(
                UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V1))?,
                RemoteStyle::Counterpart,
            )?;

            assert_eq!(negotiated(&compat), ProtocolVersionKind::V2);
            assert_eq!(compat.active_wire_version(), ProtocolVersionKind::V2);
            assert_eq!(
                initialization_state(&compat),
                InitializationState::Initializing
            );

            compat.incoming_response(
                "initialize",
                false,
                Ok(serde_json::to_value(v2_initialize_response(
                    ProtocolVersion::V2,
                ))?),
            )?;

            assert_eq!(negotiated(&compat), ProtocolVersionKind::V2);
            assert_eq!(compat.active_wire_version(), ProtocolVersionKind::V2);
            assert_eq!(initialization_state(&compat), InitializationState::Ready);
            Ok(())
        }

        #[test]
        fn failed_incoming_initialize_response_clears_pending_wire_version()
        -> Result<(), crate::Error> {
            let compat = ProtocolCompat::new(ProtocolMode::v2_client());
            assert_eq!(compat.active_wire_version(), ProtocolVersionKind::V2);

            compat.outgoing_message(
                UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V1))?,
                RemoteStyle::Counterpart,
            )?;

            assert_eq!(negotiated(&compat), ProtocolVersionKind::V2);
            assert_eq!(compat.active_wire_version(), ProtocolVersionKind::V2);

            let result = compat.incoming_response(
                "initialize",
                false,
                Err(crate::Error::invalid_request().data("initialize failed")),
            );

            assert!(result.is_err());
            assert_eq!(negotiated(&compat), ProtocolVersionKind::V2);
            assert_eq!(compat.active_wire_version(), ProtocolVersionKind::V2);
            assert_eq!(
                initialization_state(&compat),
                InitializationState::Uninitialized
            );
            compat.outgoing_message(
                UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
                RemoteStyle::Counterpart,
            )?;
            assert_eq!(
                initialization_state(&compat),
                InitializationState::Initializing
            );
            Ok(())
        }

        #[test]
        fn incoming_initialize_response_requires_protocol_version() -> Result<(), crate::Error> {
            for value in [
                serde_json::json!({}),
                serde_json::json!({ "protocolVersion": 100_000 }),
            ] {
                let compat = ProtocolCompat::new(ProtocolMode::v2_client());
                compat.outgoing_message(
                    UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V1))?,
                    RemoteStyle::Counterpart,
                )?;

                let error = compat
                    .incoming_response("initialize", false, Ok(value))
                    .expect_err("initialize responses must declare an ACP protocol version");
                let data = error
                    .data
                    .as_ref()
                    .and_then(|data| data.as_str())
                    .unwrap_or_default();
                assert!(data.contains("protocolVersion"), "{error:?}");
                assert_eq!(negotiated(&compat), ProtocolVersionKind::V2);
                assert_eq!(compat.active_wire_version(), ProtocolVersionKind::V2);
                assert_eq!(
                    initialization_state(&compat),
                    InitializationState::Uninitialized
                );
            }

            Ok(())
        }

        /// Send a v2 client's initialize, then deliver `response` to it.
        /// `negotiation` marks the request as sent by `send_initialize`.
        fn v2_client_receiving(
            negotiation: bool,
            response: Result<serde_json::Value, crate::Error>,
        ) -> Result<(ProtocolCompat, Result<serde_json::Value, crate::Error>), crate::Error>
        {
            let compat = ProtocolCompat::new(ProtocolMode::v2_client());
            compat.outgoing_message(
                UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
                RemoteStyle::Counterpart,
            )?;
            let result = compat.incoming_response("initialize", negotiation, response);
            Ok((compat, result))
        }

        fn v1_initialize_response() -> crate::schema::v1::InitializeResponse {
            crate::schema::v1::InitializeResponse::new(ProtocolVersion::V1)
                .agent_info(crate::schema::v1::Implementation::new("v1-agent", "1.0.0"))
        }

        #[test]
        fn plain_initialize_request_rejects_lower_version() -> Result<(), crate::Error> {
            let (compat, result) =
                v2_client_receiving(false, Ok(serde_json::to_value(v1_initialize_response())?))?;
            let error = result.expect_err("send_request keeps the local version guard");
            let data = error
                .data
                .as_ref()
                .and_then(|data| data.as_str())
                .unwrap_or_default();
            assert!(data.contains("peer negotiated 1"), "{error:?}");
            assert_eq!(
                initialization_state(&compat),
                InitializationState::Uninitialized
            );
            Ok(())
        }

        #[test]
        fn initialize_negotiation_receives_lower_version_as_success() -> Result<(), crate::Error> {
            let v1_response = serde_json::to_value(v1_initialize_response())?;
            let (compat, result) = v2_client_receiving(true, Ok(v1_response.clone()))?;
            assert_eq!(result?, v1_response);

            // The connection stays uninitialized rather than switching to v1.
            assert_eq!(
                initialization_state(&compat),
                InitializationState::Uninitialized
            );
            assert_eq!(compat.active_wire_version(), ProtocolVersionKind::V2);
            compat
                .outgoing_message(
                    UntypedMessage::new("session/new", serde_json::json!({}))?,
                    RemoteStyle::Counterpart,
                )
                .expect_err("v2 traffic still requires a v2 handshake");
            compat.outgoing_message(
                UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
                RemoteStyle::Counterpart,
            )?;
            Ok(())
        }

        #[test]
        fn initialize_negotiation_keeps_failures_as_errors() -> Result<(), crate::Error> {
            let remote = crate::util::unsupported_protocol_version(
                ProtocolVersion::V2,
                &[ProtocolVersion::V1],
            );
            let (compat, result) = v2_client_receiving(true, Err(remote.clone()))?;
            assert_eq!(result, Err(remote));
            assert_eq!(
                initialization_state(&compat),
                InitializationState::Uninitialized
            );

            for malformed in [
                serde_json::json!({}),
                serde_json::json!({ "protocolVersion": "one" }),
                serde_json::json!({ "protocolVersion": 0 }),
            ] {
                let (compat, result) = v2_client_receiving(true, Ok(malformed))?;
                result.expect_err("malformed initialize responses are errors");
                assert_eq!(
                    initialization_state(&compat),
                    InitializationState::Uninitialized
                );
            }
            Ok(())
        }

        #[test]
        fn initialize_negotiation_does_not_change_proxies() -> Result<(), crate::Error> {
            let compat = ProtocolCompat::new(ProtocolMode::v2_proxy());
            compat.outgoing_message(
                UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
                RemoteStyle::Successor,
            )?;
            compat
                .incoming_response(
                    "initialize",
                    true,
                    Ok(serde_json::to_value(v1_initialize_response())?),
                )
                .expect_err("only a client's own handshake can report a lower version");
            Ok(())
        }

        #[test]
        fn initialize_outcome_parses_by_selected_version() -> Result<(), crate::Error> {
            use crate::JsonRpcResponse as _;

            let v2_response = v2_initialize_response(ProtocolVersion::V2);
            assert_eq!(
                super::super::InitializeOutcome::from_value(
                    "initialize",
                    serde_json::to_value(&v2_response)?
                )?,
                super::super::InitializeOutcome::Initialized(v2_response)
            );

            let v1_response = v1_initialize_response();
            let outcome = super::super::InitializeOutcome::from_value(
                "initialize",
                serde_json::to_value(&v1_response)?,
            )?;
            assert_eq!(
                outcome,
                super::super::InitializeOutcome::LowerVersion(
                    super::super::InitializationVersionMismatch::new(
                        ProtocolVersion::V2,
                        super::super::SelectedInitializeResponse::V1(v1_response),
                    )
                )
            );
            let super::super::InitializeOutcome::LowerVersion(mismatch) = &outcome else {
                unreachable!();
            };
            assert_eq!(mismatch.requested, ProtocolVersion::V2);
            assert_eq!(mismatch.selected, ProtocolVersion::V1);

            for value in [
                serde_json::json!({}),
                serde_json::json!({ "protocolVersion": 3 }),
            ] {
                super::super::InitializeOutcome::from_value("initialize", value)
                    .expect_err("only v2 and lower known versions are outcomes");
            }
            Ok(())
        }

        #[test]
        fn malformed_v2_initialize_requests_do_not_begin_a_handshake() -> Result<(), crate::Error> {
            let malformed = serde_json::json!({ "protocolVersion": ProtocolVersion::V2 });

            let client = ProtocolCompat::new(ProtocolMode::v2_client());
            client
                .outgoing_message(
                    UntypedMessage::new("initialize", malformed.clone())?,
                    RemoteStyle::Counterpart,
                )
                .expect_err("native v2 clients must send the complete initialize request shape");
            assert_eq!(
                initialization_state(&client),
                InitializationState::Uninitialized
            );
            client.outgoing_message(
                UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
                RemoteStyle::Counterpart,
            )?;
            assert_eq!(
                initialization_state(&client),
                InitializationState::Initializing
            );

            let agent = ProtocolCompat::new(ProtocolMode::v2_agent());
            agent
                .incoming_message(UntypedMessage::new("initialize", malformed)?)
                .expect_err("native v2 agents must receive the complete initialize request shape");
            assert_eq!(
                initialization_state(&agent),
                InitializationState::Uninitialized
            );
            agent.incoming_message(UntypedMessage::new(
                "initialize",
                v2_initialize_request(ProtocolVersion::V2),
            )?)?;
            assert_eq!(
                initialization_state(&agent),
                InitializationState::Initializing
            );
            Ok(())
        }

        #[test]
        fn malformed_v2_initialize_success_leaves_client_uninitialized_for_retry()
        -> Result<(), crate::Error> {
            let compat = ProtocolCompat::new(ProtocolMode::v2_client());
            compat.outgoing_message(
                UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
                RemoteStyle::Counterpart,
            )?;

            compat
                .incoming_response(
                    "initialize",
                    false,
                    Ok(serde_json::json!({ "protocolVersion": ProtocolVersion::V2 })),
                )
                .expect_err("initialize success must contain the complete v2 response shape");
            assert_eq!(
                initialization_state(&compat),
                InitializationState::Uninitialized
            );
            assert_eq!(pending_initialize(&compat), None);

            compat.outgoing_message(
                UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
                RemoteStyle::Counterpart,
            )?;
            assert_eq!(
                initialization_state(&compat),
                InitializationState::Initializing
            );
            Ok(())
        }

        #[test]
        fn malformed_v2_initialize_success_leaves_agent_uninitialized_for_retry()
        -> Result<(), crate::Error> {
            let compat = ProtocolCompat::new(ProtocolMode::v2_agent());
            compat.incoming_message(UntypedMessage::new(
                "initialize",
                v2_initialize_request(ProtocolVersion::V2),
            )?)?;

            compat
                .outgoing_response(
                    "initialize",
                    Ok(serde_json::json!({ "protocolVersion": ProtocolVersion::V2 })),
                )
                .expect_err("initialize success must contain the complete v2 response shape");
            assert_eq!(
                initialization_state(&compat),
                InitializationState::Uninitialized
            );
            assert_eq!(pending_initialize(&compat), None);

            compat.incoming_message(UntypedMessage::new(
                "initialize",
                v2_initialize_request(ProtocolVersion::V2),
            )?)?;
            assert_eq!(
                initialization_state(&compat),
                InitializationState::Initializing
            );
            Ok(())
        }

        #[test]
        fn v2_peer_traffic_requires_completed_initialization() -> Result<(), crate::Error> {
            for compat in [
                ProtocolCompat::new(ProtocolMode::v2_agent()),
                ProtocolCompat::new(ProtocolMode::v2_client()),
            ] {
                for error in [
                    compat
                        .incoming_message(UntypedMessage::new(
                            "session/new",
                            serde_json::json!({}),
                        )?)
                        .expect_err("incoming requests must wait for initialization"),
                    compat
                        .outgoing_message(
                            UntypedMessage::new("session/new", serde_json::json!({}))?,
                            RemoteStyle::Counterpart,
                        )
                        .expect_err("outgoing requests must wait for initialization"),
                    compat
                        .incoming_notification(UntypedMessage::new(
                            "session/update",
                            serde_json::json!({}),
                        )?)
                        .expect_err("incoming notifications must wait for initialization"),
                    compat
                        .outgoing_notification(UntypedMessage::new(
                            "session/update",
                            serde_json::json!({}),
                        )?)
                        .expect_err("outgoing notifications must wait for initialization"),
                    compat
                        .incoming_response("session/new", false, Ok(serde_json::json!({})))
                        .expect_err("incoming responses must wait for initialization"),
                    compat
                        .outgoing_response("session/new", Ok(serde_json::json!({})))
                        .expect_err("outgoing responses must wait for initialization"),
                ] {
                    let data = error
                        .data
                        .as_ref()
                        .and_then(|data| data.as_str())
                        .unwrap_or_default();
                    assert!(data.contains("initialization must complete"), "{error:?}");
                }
            }
            Ok(())
        }

        #[test]
        fn v2_initialization_allows_only_protocol_cancellation_notifications()
        -> Result<(), crate::Error> {
            let client = ProtocolCompat::new(ProtocolMode::v2_client());
            client.outgoing_message(
                UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
                RemoteStyle::Counterpart,
            )?;
            client.outgoing_notification(UntypedMessage::new(
                "$/cancel_request",
                serde_json::json!({ "requestId": 1 }),
            )?)?;
            client
                .outgoing_notification(UntypedMessage::new(
                    "session/update",
                    serde_json::json!({}),
                )?)
                .expect_err("ordinary notifications must wait for initialization");

            let agent = ProtocolCompat::new(ProtocolMode::v2_agent());
            agent.incoming_message(UntypedMessage::new(
                "initialize",
                v2_initialize_request(ProtocolVersion::V2),
            )?)?;
            agent.incoming_notification(UntypedMessage::new(
                "$/cancel_request",
                serde_json::json!({ "requestId": 1 }),
            )?)?;
            agent
                .incoming_notification(UntypedMessage::new(
                    "session/update",
                    serde_json::json!({}),
                )?)
                .expect_err("ordinary notifications must wait for initialization");
            Ok(())
        }

        fn error_data(error: &crate::Error) -> &str {
            error
                .data
                .as_ref()
                .and_then(|data| data.as_str())
                .unwrap_or_default()
        }

        #[test]
        fn v2_peer_initialization_has_one_direction() -> Result<(), crate::Error> {
            let agent = ProtocolCompat::new(ProtocolMode::v2_agent());
            let error = agent
                .outgoing_message(
                    UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
                    RemoteStyle::Counterpart,
                )
                .expect_err("agents must not initiate initialization");
            assert!(
                error_data(&error).contains("clients send `initialize`"),
                "{error:?}"
            );
            assert_eq!(
                initialization_state(&agent),
                InitializationState::Uninitialized
            );

            let client = ProtocolCompat::new(ProtocolMode::v2_client());
            let error = client
                .incoming_message(UntypedMessage::new(
                    "initialize",
                    v2_initialize_request(ProtocolVersion::V2),
                )?)
                .expect_err("clients must not receive initialization requests");
            assert!(
                error_data(&error).contains("clients send `initialize`"),
                "{error:?}"
            );
            assert_eq!(
                initialization_state(&client),
                InitializationState::Uninitialized
            );
            Ok(())
        }

        /// Drive an agent through one successful handshake.
        fn initialized_v2_agent() -> Result<ProtocolCompat, crate::Error> {
            let agent = ProtocolCompat::new(ProtocolMode::v2_agent());
            agent.incoming_message(UntypedMessage::new(
                "initialize",
                v2_initialize_request(ProtocolVersion::V2),
            )?)?;
            agent.outgoing_response(
                "initialize",
                Ok(serde_json::to_value(v2_initialize_response(
                    ProtocolVersion::V2,
                ))?),
            )?;
            Ok(agent)
        }

        /// Drive a client through one successful handshake.
        fn initialized_v2_client() -> Result<ProtocolCompat, crate::Error> {
            let client = ProtocolCompat::new(ProtocolMode::v2_client());
            client.outgoing_message(
                UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
                RemoteStyle::Counterpart,
            )?;
            client.incoming_response(
                "initialize",
                false,
                Ok(serde_json::to_value(v2_initialize_response(
                    ProtocolVersion::V2,
                ))?),
            )?;
            Ok(client)
        }

        #[test]
        fn v2_agent_accepts_repeated_initialize_before_other_requests() -> Result<(), crate::Error>
        {
            let agent = initialized_v2_agent()?;
            assert_eq!(initialization_state(&agent), InitializationState::Ready);

            // Notifications do not close the window; only requests do.
            agent.incoming_notification(UntypedMessage::new(
                "$/cancel_request",
                serde_json::json!({ "requestId": 1 }),
            )?)?;

            for _ in 0..2 {
                agent.incoming_message(UntypedMessage::new(
                    "initialize",
                    v2_initialize_request(ProtocolVersion::V2),
                )?)?;
                assert_eq!(
                    initialization_state(&agent),
                    InitializationState::Initializing
                );
                agent
                    .incoming_message(UntypedMessage::new("session/new", serde_json::json!({}))?)
                    .expect_err("requests must wait for the repeated initialize");
                agent.outgoing_response(
                    "initialize",
                    Ok(serde_json::to_value(v2_initialize_response(
                        ProtocolVersion::V2,
                    ))?),
                )?;
                assert_eq!(initialization_state(&agent), InitializationState::Ready);
                assert_eq!(negotiated(&agent), ProtocolVersionKind::V2);
            }

            agent.incoming_message(UntypedMessage::new("session/new", serde_json::json!({}))?)?;
            let error = agent
                .incoming_message(UntypedMessage::new(
                    "initialize",
                    v2_initialize_request(ProtocolVersion::V2),
                )?)
                .expect_err("initialize must not be repeated after another request");
            assert_eq!(i32::from(error.code), -32600);
            assert!(
                error_data(&error).contains("cannot be repeated after any other request"),
                "{error:?}"
            );
            assert_eq!(initialization_state(&agent), InitializationState::Ready);
            Ok(())
        }

        #[test]
        fn v2_agent_keeps_window_open_when_a_request_is_rejected() -> Result<(), crate::Error> {
            let agent = ProtocolCompat::new(ProtocolMode::v2_agent());
            agent.incoming_message(UntypedMessage::new(
                "initialize",
                v2_initialize_request(ProtocolVersion::V2),
            )?)?;
            // Rejected while the handshake is in flight, so it does not count.
            agent
                .incoming_message(UntypedMessage::new("session/new", serde_json::json!({}))?)
                .expect_err("requests must wait for initialization");
            agent.outgoing_response(
                "initialize",
                Ok(serde_json::to_value(v2_initialize_response(
                    ProtocolVersion::V2,
                ))?),
            )?;
            agent.incoming_message(UntypedMessage::new(
                "initialize",
                v2_initialize_request(ProtocolVersion::V2),
            )?)?;
            assert_eq!(
                initialization_state(&agent),
                InitializationState::Initializing
            );
            Ok(())
        }

        #[test]
        fn failed_repeated_initialize_keeps_previous_initialization() -> Result<(), crate::Error> {
            let agent = initialized_v2_agent()?;
            agent.incoming_message(UntypedMessage::new(
                "initialize",
                v2_initialize_request(ProtocolVersion::V2),
            )?)?;
            agent
                .outgoing_response(
                    "initialize",
                    Err(crate::Error::invalid_params().data("rejected")),
                )
                .expect_err("the handler's error is forwarded");
            assert_eq!(initialization_state(&agent), InitializationState::Ready);

            // A malformed repeat is rejected before it begins.
            agent
                .incoming_message(UntypedMessage::new(
                    "initialize",
                    serde_json::json!({ "protocolVersion": 2 }),
                )?)
                .expect_err("an exact v2 repeat must be a valid v2 request");
            assert_eq!(initialization_state(&agent), InitializationState::Ready);

            // The client may still repeat.
            agent.incoming_message(UntypedMessage::new(
                "initialize",
                v2_initialize_request(ProtocolVersion::V2),
            )?)?;
            agent.outgoing_response(
                "initialize",
                Ok(serde_json::to_value(v2_initialize_response(
                    ProtocolVersion::V2,
                ))?),
            )?;
            agent.incoming_message(UntypedMessage::new("session/new", serde_json::json!({}))?)?;
            Ok(())
        }

        #[test]
        fn v2_agent_rejects_lower_version_repeat_without_losing_ready_state()
        -> Result<(), crate::Error> {
            let agent = initialized_v2_agent()?;
            let error = agent
                .incoming_message(UntypedMessage::new(
                    "initialize",
                    serde_json::json!({ "protocolVersion": 1, "clientCapabilities": {} }),
                )?)
                .expect_err("a repeat must not request a version below the selected one");
            assert_eq!(i32::from(error.code), -32600);
            assert_eq!(initialization_state(&agent), InitializationState::Ready);
            agent.incoming_message(UntypedMessage::new(
                "initialize",
                v2_initialize_request(ProtocolVersion::V2),
            )?)?;
            Ok(())
        }

        #[test]
        fn v2_client_may_repeat_initialize_before_other_requests() -> Result<(), crate::Error> {
            let client = initialized_v2_client()?;
            client.outgoing_message(
                UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
                RemoteStyle::Counterpart,
            )?;
            assert_eq!(
                initialization_state(&client),
                InitializationState::Initializing
            );
            client.incoming_response(
                "initialize",
                false,
                Ok(serde_json::to_value(v2_initialize_response(
                    ProtocolVersion::V2,
                ))?),
            )?;
            assert_eq!(initialization_state(&client), InitializationState::Ready);

            // Requests from the agent do not close the client's window.
            client.incoming_message(UntypedMessage::new(
                "session/request_permission",
                serde_json::json!({}),
            )?)?;
            client.outgoing_message(
                UntypedMessage::new("session/new", serde_json::json!({}))?,
                RemoteStyle::Counterpart,
            )?;
            let error = client
                .outgoing_message(
                    UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
                    RemoteStyle::Counterpart,
                )
                .expect_err("initialize must not be repeated after another request");
            assert!(
                error_data(&error).contains("cannot be repeated after any other request"),
                "{error:?}"
            );
            assert_eq!(initialization_state(&client), InitializationState::Ready);
            Ok(())
        }

        #[test]
        fn concurrent_initialize_is_rejected_for_first_and_repeated_handshakes()
        -> Result<(), crate::Error> {
            for agent in [
                ProtocolCompat::new(ProtocolMode::v2_agent()),
                initialized_v2_agent()?,
            ] {
                agent.incoming_request(
                    &RequestId::Number(1),
                    UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
                )?;
                let error = agent
                    .incoming_request(
                        &RequestId::Number(2),
                        UntypedMessage::new(
                            "initialize",
                            v2_initialize_request(ProtocolVersion::V2),
                        )?,
                    )
                    .expect_err("only one initialize may be in flight");
                assert_eq!(i32::from(error.code), -32600);
                assert!(error_data(&error).contains("in progress"), "{error:?}");
            }
            Ok(())
        }

        #[test]
        fn v2_connections_reject_null_request_ids() -> Result<(), crate::Error> {
            for compat in [
                ProtocolCompat::new(ProtocolMode::v2_agent()),
                ProtocolCompat::new(ProtocolMode::v2_proxy()),
                initialized_v2_agent()?,
                initialized_v2_client()?,
            ] {
                for method in ["initialize", "_proxy/initialize", "session/new"] {
                    let state = initialization_state(&compat);
                    let error = compat
                        .incoming_request(
                            &RequestId::Null,
                            UntypedMessage::new(
                                method,
                                v2_initialize_request(ProtocolVersion::V2),
                            )?,
                        )
                        .expect_err("v2 request ids must not be null");
                    assert_eq!(i32::from(error.code), -32600);
                    assert!(error_data(&error).contains("not null"), "{error:?}");
                    assert_eq!(initialization_state(&compat), state);
                    assert_eq!(pending_initialize(&compat), None);
                }
            }

            // V1 keeps accepting null ids.
            let v1_agent = ProtocolCompat::new(ProtocolMode::v1_agent());
            v1_agent.incoming_request(
                &RequestId::Null,
                UntypedMessage::new(
                    "initialize",
                    serde_json::json!({ "protocolVersion": 1, "clientCapabilities": {} }),
                )?,
            )?;
            Ok(())
        }

        #[test]
        fn newer_version_initialize_keeps_what_parses_under_v2() -> Result<(), crate::Error> {
            let agent = ProtocolCompat::new(ProtocolMode::v2_agent());
            let request = agent.incoming_message(UntypedMessage::new(
                "initialize",
                serde_json::json!({
                    "protocolVersion": 3,
                    "info": { "name": "future-client" },
                    "capabilities": {
                        "auth": "malformed",
                        "elicitation": {},
                        "_futureCapability": { "dropped": true }
                    },
                    "_meta": "malformed",
                    "_futureField": true
                }),
            )?)?;
            let initialize: v2::InitializeRequest =
                serde_json::from_value(request.params().clone())?;
            assert_eq!(initialize.protocol_version, ProtocolVersion::V2);
            assert_eq!(
                initialize.info,
                v2::Implementation::new("unknown", "unknown")
            );
            assert_eq!(initialize.capabilities.auth, None);
            assert!(initialize.capabilities.elicitation.is_some());
            assert_eq!(initialize.meta, None);
            assert_eq!(request.params(), &serde_json::to_value(&initialize)?);

            // A parsable `info` and `_meta` are kept, and capabilities that
            // are not an object are dropped as a whole.
            let agent = ProtocolCompat::new(ProtocolMode::v2_agent());
            let request = agent.incoming_message(UntypedMessage::new(
                "initialize",
                serde_json::json!({
                    "protocolVersion": 3,
                    "info": { "name": "future-client", "version": "3.0.0" },
                    "capabilities": [],
                    "_meta": { "kept": true }
                }),
            )?)?;
            let initialize: v2::InitializeRequest =
                serde_json::from_value(request.params().clone())?;
            assert_eq!(
                initialize.info,
                v2::Implementation::new("future-client", "3.0.0")
            );
            assert_eq!(initialize.capabilities, v2::ClientCapabilities::new());
            assert_eq!(
                request.params()["_meta"],
                serde_json::json!({ "kept": true })
            );
            Ok(())
        }

        #[test]
        fn exact_version_initialize_stays_strict() -> Result<(), crate::Error> {
            let agent = ProtocolCompat::new(ProtocolMode::v2_agent());
            let error = agent
                .incoming_message(UntypedMessage::new(
                    "initialize",
                    serde_json::json!({ "protocolVersion": 2, "info": { "name": "client" } }),
                )?)
                .expect_err("an exact v2 request must carry a valid `info`");
            assert_eq!(i32::from(error.code), -32602);
            assert_eq!(
                initialization_state(&agent),
                InitializationState::Uninitialized
            );
            Ok(())
        }

        #[test]
        fn v1_downgrade_keeps_what_parses_under_v1() -> Result<(), crate::Error> {
            let agent = ProtocolCompat::new(ProtocolMode::v1_agent());
            let request = agent.incoming_message(UntypedMessage::new(
                "initialize",
                serde_json::json!({
                    "protocolVersion": 2,
                    "info": "malformed",
                    "capabilities": { "elicitation": {} },
                    "_meta": 7
                }),
            )?)?;
            let initialize: crate::schema::v1::InitializeRequest =
                serde_json::from_value(request.params().clone())?;
            assert_eq!(initialize.protocol_version, ProtocolVersion::V1);
            assert_eq!(initialize.client_info, None);
            assert!(initialize.client_capabilities.elicitation.is_some());
            assert_eq!(initialize.meta, None);
            assert!(request.params().get("clientInfo").is_none());
            assert!(request.params().get("_meta").is_none());

            let agent = ProtocolCompat::new(ProtocolMode::v1_agent());
            let request = agent.incoming_message(UntypedMessage::new(
                "initialize",
                serde_json::json!({ "protocolVersion": 2 }),
            )?)?;
            let initialize: crate::schema::v1::InitializeRequest =
                serde_json::from_value(request.params().clone())?;
            assert_eq!(initialize.client_info, None);
            Ok(())
        }

        #[test]
        fn rejected_concurrent_initialize_does_not_clear_the_active_handshake()
        -> Result<(), crate::Error> {
            let compat = ProtocolCompat::new(ProtocolMode::v2_agent());
            let accepted_id = RequestId::Number(1);
            let rejected_id = RequestId::Number(2);

            compat.incoming_request(
                &accepted_id,
                UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
            )?;
            let duplicate_error = compat
                .incoming_request(
                    &rejected_id,
                    UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
                )
                .expect_err("a second initialize request must be rejected while one is active");
            compat
                .outgoing_response_to(&rejected_id, "initialize", Err(duplicate_error))
                .expect_err("the rejected initialize receives its own error response");

            assert_eq!(
                initialization_state(&compat),
                InitializationState::Initializing
            );
            assert_eq!(pending_initialize(&compat), Some(ProtocolVersionKind::V2));

            compat.outgoing_response_to(
                &accepted_id,
                "initialize",
                Ok(serde_json::to_value(v2_initialize_response(
                    ProtocolVersion::V2,
                ))?),
            )?;
            assert_eq!(initialization_state(&compat), InitializationState::Ready);
            Ok(())
        }

        #[test]
        fn rejected_wrong_direction_initialize_does_not_clear_client_handshake()
        -> Result<(), crate::Error> {
            let compat = ProtocolCompat::new(ProtocolMode::v2_client());
            compat.outgoing_message(
                UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
                RemoteStyle::Counterpart,
            )?;

            let wrong_direction_id = RequestId::Number(2);
            let wrong_direction_error = compat
                .incoming_request(
                    &wrong_direction_id,
                    UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
                )
                .expect_err("agents must not send initialize requests to clients");
            compat
                .outgoing_response_to(
                    &wrong_direction_id,
                    "initialize",
                    Err(wrong_direction_error),
                )
                .expect_err("the wrong-direction initialize receives its own error response");

            assert_eq!(
                initialization_state(&compat),
                InitializationState::Initializing
            );
            assert_eq!(pending_initialize(&compat), Some(ProtocolVersionKind::V2));

            compat.incoming_response(
                "initialize",
                false,
                Ok(serde_json::to_value(v2_initialize_response(
                    ProtocolVersion::V2,
                ))?),
            )?;
            assert_eq!(initialization_state(&compat), InitializationState::Ready);
            Ok(())
        }

        #[test]
        fn incoming_initialize_request_negotiates_newer_protocol_versions()
        -> Result<(), crate::Error> {
            for requested in [2_u16, 3, u16::MAX] {
                let compat = ProtocolCompat::new(ProtocolMode::v2_agent());
                let request = compat.incoming_message(UntypedMessage::new(
                    "initialize",
                    v2_initialize_request(ProtocolVersion::from(requested)),
                )?)?;
                assert_eq!(request.params()["protocolVersion"], serde_json::json!(2));
                assert_eq!(pending_initialize(&compat), Some(ProtocolVersionKind::V2));

                let response = compat.outgoing_response(
                    "initialize",
                    Ok(serde_json::to_value(v2_initialize_response(
                        ProtocolVersion::V2,
                    ))?),
                )?;
                assert_eq!(response["protocolVersion"], serde_json::json!(2));
                assert_eq!(initialization_state(&compat), InitializationState::Ready);
                assert_eq!(compat.active_wire_version(), ProtocolVersionKind::V2);
            }
            Ok(())
        }

        #[test]
        fn incoming_initialize_request_rejects_unsupported_protocol_version()
        -> Result<(), crate::Error> {
            let compat = ProtocolCompat::new(ProtocolMode::v2_agent());
            let error = compat
                .incoming_message(UntypedMessage::new(
                    "initialize",
                    serde_json::json!({ "protocolVersion": 1, "clientCapabilities": {} }),
                )?)
                .expect_err("v2 agents should reject v1 initialization without a v1 handler");
            assert_eq!(i32::from(error.code), -32022);
            assert_eq!(
                error.data,
                Some(serde_json::json!({ "requested": 1, "supported": [2] }))
            );
            assert_eq!(negotiated(&compat), ProtocolVersionKind::V2);
            assert_eq!(compat.active_wire_version(), ProtocolVersionKind::V2);
            assert_eq!(
                initialization_state(&compat),
                InitializationState::Uninitialized
            );

            Ok(())
        }

        #[test]
        fn proxy_initialize_request_requires_the_selected_protocol_version()
        -> Result<(), crate::Error> {
            for (mode, selected, unsupported, selected_kind) in [
                (
                    ProtocolMode::v1_proxy(),
                    ProtocolVersion::V1,
                    ProtocolVersion::V2,
                    ProtocolVersionKind::V1,
                ),
                (
                    ProtocolMode::v2_proxy(),
                    ProtocolVersion::V2,
                    ProtocolVersion::V1,
                    ProtocolVersionKind::V2,
                ),
            ] {
                let compat = ProtocolCompat::new(mode);
                let error = compat
                    .incoming_message(UntypedMessage::new(
                        "_proxy/initialize",
                        serde_json::json!({ "protocolVersion": unsupported }),
                    )?)
                    .expect_err(
                        "proxy initialization must reject a protocol version outside its API",
                    );
                let data = error
                    .data
                    .as_ref()
                    .and_then(|data| data.as_str())
                    .unwrap_or_default();
                assert!(
                    data.contains(&format!("only supports ACP protocol version {selected}")),
                    "{error:?}"
                );
                assert_eq!(negotiated(&compat), selected_kind);
                assert_eq!(pending_initialize(&compat), None);
                assert_eq!(compat.active_wire_version(), selected_kind);
            }

            Ok(())
        }

        #[test]
        fn proxy_initialize_requests_reject_wrong_directions_and_wrapping()
        -> Result<(), crate::Error> {
            for mode in [ProtocolMode::v1_proxy(), ProtocolMode::v2_proxy()] {
                let compat = ProtocolCompat::new(mode);
                for error in [
                    compat
                        .incoming_message(UntypedMessage::new(
                            "initialize",
                            serde_json::json!({ "protocolVersion": ProtocolVersion::V2 }),
                        )?)
                        .expect_err("proxy initialization must arrive through `_proxy/initialize`"),
                    compat
                        .incoming_message(UntypedMessage::new(
                            "_proxy/successor",
                            serde_json::json!({
                                "method": "initialize",
                                "params": { "protocolVersion": ProtocolVersion::V2 }
                            }),
                        )?)
                        .expect_err("successor traffic must not carry initialization"),
                    compat
                        .incoming_message(UntypedMessage::new(
                            "_proxy/successor",
                            serde_json::json!({
                                "method": "_proxy/successor",
                                "params": {
                                    "method": "_proxy/initialize",
                                    "params": { "protocolVersion": ProtocolVersion::V2 }
                                }
                            }),
                        )?)
                        .expect_err("nested successor traffic must not carry initialization"),
                    compat
                        .outgoing_message(
                            UntypedMessage::new(
                                "initialize",
                                serde_json::json!({ "protocolVersion": ProtocolVersion::V2 }),
                            )?,
                            RemoteStyle::Predecessor,
                        )
                        .expect_err("downstream proxy initialization must use `_proxy/successor`"),
                    compat
                        .outgoing_message(
                            UntypedMessage::new(
                                "_proxy/initialize",
                                serde_json::json!({ "protocolVersion": ProtocolVersion::V2 }),
                            )?,
                            RemoteStyle::Successor,
                        )
                        .expect_err("a proxy must not send `_proxy/initialize` downstream"),
                ] {
                    let data = error
                        .data
                        .as_ref()
                        .and_then(|data| data.as_str())
                        .unwrap_or_default();
                    assert!(data.contains("_proxy/initialize"), "{error:?}");
                    assert!(data.contains("_proxy/successor"), "{error:?}");
                }
                assert_eq!(pending_initialize(&compat), None);
            }

            Ok(())
        }

        #[test]
        fn outgoing_initialize_rejects_non_object_params() -> Result<(), crate::Error> {
            for (compat, message, remote_style) in [
                (
                    ProtocolCompat::new(ProtocolMode::v2_client()),
                    UntypedMessage::new("initialize", serde_json::Value::Null)?,
                    RemoteStyle::Counterpart,
                ),
                (
                    ProtocolCompat::new(ProtocolMode::v2_proxy()),
                    UntypedMessage::new("initialize", serde_json::Value::Null)?,
                    RemoteStyle::Successor,
                ),
            ] {
                let error = compat
                    .outgoing_message(message, remote_style)
                    .expect_err("initialize params must be an object");
                let data = error
                    .data
                    .as_ref()
                    .and_then(|data| data.as_str())
                    .unwrap_or_default();
                assert!(data.contains("protocolVersion"), "{error:?}");
                assert_eq!(pending_initialize(&compat), None);
            }

            Ok(())
        }

        #[test]
        fn outgoing_initialize_rejects_explicit_successor_wrapping() -> Result<(), crate::Error> {
            for message in [
                UntypedMessage::new(
                    "_proxy/successor",
                    serde_json::json!({
                        "method": "initialize",
                        "params": { "protocolVersion": ProtocolVersion::V1 }
                    }),
                )?,
                UntypedMessage::new(
                    "_proxy/successor",
                    serde_json::json!({
                        "method": "_proxy/successor",
                        "params": {
                            "method": "initialize",
                            "params": { "protocolVersion": ProtocolVersion::V1 }
                        }
                    }),
                )?,
            ] {
                let compat = ProtocolCompat::new(ProtocolMode::v2_proxy());
                let error = compat
                    .outgoing_message(message, RemoteStyle::Successor)
                    .expect_err("connection routing must own successor wrapping");
                let data = error
                    .data
                    .as_ref()
                    .and_then(|data| data.as_str())
                    .unwrap_or_default();
                assert!(data.contains("logical `initialize`"), "{error:?}");
                assert!(data.contains("_proxy/successor"), "{error:?}");
                assert_eq!(pending_initialize(&compat), None);
            }

            Ok(())
        }

        #[test]
        fn proxy_initialize_round_trip_uses_the_selected_protocol_version()
        -> Result<(), crate::Error> {
            for (mode, selected, selected_kind) in [
                (
                    ProtocolMode::v1_proxy(),
                    ProtocolVersion::V1,
                    ProtocolVersionKind::V1,
                ),
                (
                    ProtocolMode::v2_proxy(),
                    ProtocolVersion::V2,
                    ProtocolVersionKind::V2,
                ),
            ] {
                let compat = ProtocolCompat::new(mode);
                let request = compat.incoming_message(UntypedMessage::new(
                    "_proxy/initialize",
                    serde_json::json!({ "protocolVersion": selected }),
                )?)?;
                assert_eq!(
                    required_protocol_version_from_value(request.params())?,
                    selected
                );
                assert_eq!(pending_initialize(&compat), Some(selected_kind));

                let response = compat.outgoing_response(
                    "_proxy/initialize",
                    Ok(serde_json::json!({ "protocolVersion": selected })),
                )?;
                assert_eq!(required_protocol_version_from_value(&response)?, selected);
                assert_eq!(negotiated(&compat), selected_kind);
                assert_eq!(pending_initialize(&compat), None);
                assert_eq!(compat.active_wire_version(), selected_kind);
            }

            Ok(())
        }

        #[test]
        fn proxy_initialize_response_rejects_the_wrong_version_and_clears_pending_state()
        -> Result<(), crate::Error> {
            for (mode, selected, unsupported, selected_kind) in [
                (
                    ProtocolMode::v1_proxy(),
                    ProtocolVersion::V1,
                    ProtocolVersion::V2,
                    ProtocolVersionKind::V1,
                ),
                (
                    ProtocolMode::v2_proxy(),
                    ProtocolVersion::V2,
                    ProtocolVersion::V1,
                    ProtocolVersionKind::V2,
                ),
            ] {
                let compat = ProtocolCompat::new(mode);
                let request = compat.outgoing_message(
                    UntypedMessage::new(
                        "initialize",
                        serde_json::json!({ "protocolVersion": unsupported }),
                    )?,
                    RemoteStyle::Successor,
                )?;
                assert_eq!(
                    required_protocol_version_from_value(request.params())?,
                    selected
                );
                assert_eq!(pending_initialize(&compat), Some(selected_kind));

                let error = compat
                    .incoming_response(
                        "initialize",
                        false,
                        Ok(serde_json::json!({ "protocolVersion": unsupported })),
                    )
                    .expect_err("proxy initialization must reject a mismatched response version");
                let data = error
                    .data
                    .as_ref()
                    .and_then(|data| data.as_str())
                    .unwrap_or_default();
                assert!(
                    data.contains(&format!(
                        "required ACP protocol version {selected} but peer negotiated {unsupported}"
                    )),
                    "{error:?}"
                );
                assert_eq!(negotiated(&compat), selected_kind);
                assert_eq!(pending_initialize(&compat), None);
                assert_eq!(compat.active_wire_version(), selected_kind);
            }

            Ok(())
        }

        #[test]
        #[should_panic(expected = "cannot merge ACP builders with different API protocol versions")]
        fn merging_different_api_protocol_modes_panics() {
            let _ = ProtocolMode::v1_agent().merge(ProtocolMode::v2_agent());
        }

        #[test]
        #[should_panic(expected = "cannot merge standard ACP and proxy ACP builders")]
        fn merging_standard_and_proxy_protocol_modes_panics() {
            let _ = ProtocolMode::v1_agent().merge(ProtocolMode::v1_proxy());
        }
    }
}

pub(crate) use imp::{ProtocolCompat, ProtocolMode};
