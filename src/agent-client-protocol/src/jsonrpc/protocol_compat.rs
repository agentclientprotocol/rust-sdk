#[cfg(not(feature = "unstable_protocol_v2"))]
mod imp {
    #![allow(clippy::unused_self, clippy::unnecessary_wraps)]
    use crate::schema::v1::RequestId;
    use crate::{UntypedMessage, role::RemoteStyle};

    #[derive(Clone, Copy, Debug, Default)]
    pub(crate) struct ProtocolMode;

    impl ProtocolMode {
        pub(crate) fn disabled() -> Self {
            Self
        }

        pub(crate) fn v1_agent() -> Self {
            Self
        }

        pub(crate) fn v1_client() -> Self {
            Self
        }

        pub(crate) fn v1_proxy() -> Self {
            Self
        }

        pub(crate) fn merge(self, _other: Self) -> Self {
            self
        }
    }

    #[derive(Clone, Debug, Default)]
    pub(crate) struct ProtocolCompat;

    impl ProtocolCompat {
        pub(crate) fn new(_mode: ProtocolMode) -> Self {
            Self
        }

        pub(crate) fn incoming_message(
            &self,
            message: UntypedMessage,
        ) -> Result<UntypedMessage, crate::Error> {
            Ok(message)
        }

        pub(crate) fn incoming_request(
            &self,
            _id: &RequestId,
            message: UntypedMessage,
        ) -> Result<UntypedMessage, crate::Error> {
            self.incoming_message(message)
        }

        pub(crate) fn outgoing_message(
            &self,
            message: UntypedMessage,
            _remote_style: RemoteStyle,
        ) -> Result<UntypedMessage, crate::Error> {
            Ok(message)
        }

        pub(crate) fn incoming_notification(
            &self,
            message: UntypedMessage,
        ) -> Result<Vec<UntypedMessage>, crate::Error> {
            Ok(vec![message])
        }

        pub(crate) fn outgoing_notification(
            &self,
            message: UntypedMessage,
        ) -> Result<Vec<UntypedMessage>, crate::Error> {
            Ok(vec![message])
        }

        pub(crate) fn incoming_response(
            &self,
            _method: &str,
            result: Result<serde_json::Value, crate::Error>,
        ) -> Result<serde_json::Value, crate::Error> {
            result
        }

        pub(crate) fn outgoing_response(
            &self,
            _method: &str,
            result: Result<serde_json::Value, crate::Error>,
        ) -> Result<serde_json::Value, crate::Error> {
            result
        }

        pub(crate) fn outgoing_response_to(
            &self,
            _id: &RequestId,
            method: &str,
            result: Result<serde_json::Value, crate::Error>,
        ) -> Result<serde_json::Value, crate::Error> {
            self.outgoing_response(method, result)
        }
    }
}

#[cfg(feature = "unstable_protocol_v2")]
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
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum InitializeSurface {
        Peer,
        Proxy,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
            })
        }

        pub(crate) fn v1_client() -> Self {
            Self::Acp(AcpProtocolMode {
                api: ProtocolVersionKind::V1,
                initialize_surface: InitializeSurface::Peer,
                initialization_role: InitializationRole::Unchecked,
            })
        }

        pub(crate) fn v1_proxy() -> Self {
            Self::Acp(AcpProtocolMode {
                api: ProtocolVersionKind::V1,
                initialize_surface: InitializeSurface::Proxy,
                initialization_role: InitializationRole::Unchecked,
            })
        }

        pub(crate) fn v2_agent() -> Self {
            Self::Acp(AcpProtocolMode {
                api: ProtocolVersionKind::V2,
                initialize_surface: InitializeSurface::Peer,
                initialization_role: InitializationRole::Responder,
            })
        }

        pub(crate) fn v2_client() -> Self {
            Self::Acp(AcpProtocolMode {
                api: ProtocolVersionKind::V2,
                initialize_surface: InitializeSurface::Peer,
                initialization_role: InitializationRole::Initiator,
            })
        }

        pub(crate) fn v2_proxy() -> Self {
            Self::Acp(AcpProtocolMode {
                api: ProtocolVersionKind::V2,
                initialize_surface: InitializeSurface::Proxy,
                initialization_role: InitializationRole::Unchecked,
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
                    Self::Acp(this)
                }
            }
        }

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
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum InitializationState {
        Unchecked,
        Uninitialized,
        Initializing,
        Ready,
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
                Self::V2 => ProtocolVersion::V2,
            }
        }

        fn from_protocol_version(version: ProtocolVersion) -> Option<Self> {
            if version == ProtocolVersion::V1 {
                Some(Self::V1)
            } else if version == ProtocolVersion::V2 {
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
                })),
            }
        }

        #[cfg(test)]
        pub(crate) fn incoming_message(
            &self,
            message: UntypedMessage,
        ) -> Result<UntypedMessage, crate::Error> {
            self.incoming_request(&RequestId::Null, message)
        }

        pub(crate) fn incoming_request(
            &self,
            id: &RequestId,
            message: UntypedMessage,
        ) -> Result<UntypedMessage, crate::Error> {
            let Some(mode) = self.mode else {
                return Ok(message);
            };

            if mode.is_incoming_initialize_request(message.method()) {
                return self.incoming_initialize_request(mode, id, message);
            }
            if mode.initialize_surface == InitializeSurface::Proxy
                && (message.method() == "initialize" || successor_encloses_initialize(&message))
            {
                return Err(invalid_proxy_initialize_direction());
            }

            self.ensure_initialized(message.method())?;
            ensure_matching_protocol_version(
                message.method(),
                self.active_wire_version(),
                mode.api,
            )?;
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

        pub(crate) fn incoming_response(
            &self,
            method: &str,
            result: Result<serde_json::Value, crate::Error>,
        ) -> Result<serde_json::Value, crate::Error> {
            let Some(mode) = self.mode else {
                return result;
            };

            if AcpProtocolMode::is_incoming_initialize_response(method) {
                return self.incoming_initialize_response(mode, result);
            }

            let value = result?;
            self.ensure_initialized(method)?;
            ensure_matching_protocol_version(method, self.active_wire_version(), mode.api)?;
            Ok(value)
        }

        #[cfg(test)]
        pub(crate) fn outgoing_response(
            &self,
            method: &str,
            result: Result<serde_json::Value, crate::Error>,
        ) -> Result<serde_json::Value, crate::Error> {
            self.outgoing_response_to(&RequestId::Null, method, result)
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
            let requested_kind = ProtocolVersionKind::from_protocol_version(requested)
                .ok_or_else(|| unsupported_protocol_version(requested, mode.api))?;
            if requested_kind != mode.api {
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
                if wire_version != mode.api {
                    return Err(required_protocol_version(mode.api, wire_version));
                }
                ensure_matching_protocol_version("initialize", pending, wire_version)?;
                set_protocol_version(&mut value, wire_version)?;
                validate_native_v2_initialize_response(mode, &value)?;
                Ok(wire_version)
            })();

            match response {
                Ok(wire_version) => {
                    self.complete_initialize(wire_version)?;
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
                InitializationState::Ready => Err(crate::Error::invalid_request().data(
                    "ACP connections may only be initialized once; reconnect to initialize again",
                )),
            }
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
                    Err(crate::Error::invalid_request().data(format!(
                        "ACP initialization must complete before `{method}` can be used",
                    )))
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
            if state.initialization == InitializationState::Initializing {
                state.initialization = InitializationState::Ready;
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
                state.initialization = InitializationState::Uninitialized;
            }
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

    fn validate_native_v2_initialize_request(
        mode: AcpProtocolMode,
        value: &serde_json::Value,
    ) -> Result<(), crate::Error> {
        if mode.initialization_role == InitializationRole::Unchecked {
            return Ok(());
        }
        <crate::schema::v2::InitializeRequest as crate::JsonRpcMessage>::parse_message(
            "initialize",
            value,
        )?;
        Ok(())
    }

    fn validate_native_v2_initialize_response(
        mode: AcpProtocolMode,
        value: &serde_json::Value,
    ) -> Result<(), crate::Error> {
        if mode.initialization_role == InitializationRole::Unchecked {
            return Ok(());
        }
        <crate::schema::v2::InitializeResponse as crate::JsonRpcResponse>::from_value(
            "initialize",
            value.clone(),
        )?;
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

    #[cfg(test)]
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
                    .incoming_response("initialize", Ok(value))
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
                        .incoming_response("session/new", Ok(serde_json::json!({})))
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

        #[test]
        fn v2_peer_initialization_has_one_direction_and_one_successful_round_trip()
        -> Result<(), crate::Error> {
            let agent = ProtocolCompat::new(ProtocolMode::v2_agent());
            let error = agent
                .outgoing_message(
                    UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
                    RemoteStyle::Counterpart,
                )
                .expect_err("agents must not initiate initialization");
            assert!(
                error
                    .data
                    .as_ref()
                    .and_then(|data| data.as_str())
                    .is_some_and(|data| data.contains("clients send `initialize`")),
                "{error:?}"
            );
            assert_eq!(
                initialization_state(&agent),
                InitializationState::Uninitialized
            );

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
            let error = agent
                .incoming_message(UntypedMessage::new(
                    "initialize",
                    v2_initialize_request(ProtocolVersion::V2),
                )?)
                .expect_err("agents must reject reinitialization");
            assert!(
                error
                    .data
                    .as_ref()
                    .and_then(|data| data.as_str())
                    .is_some_and(|data| data.contains("only be initialized once")),
                "{error:?}"
            );
            assert_eq!(initialization_state(&agent), InitializationState::Ready);

            let client = ProtocolCompat::new(ProtocolMode::v2_client());
            let error = client
                .incoming_message(UntypedMessage::new(
                    "initialize",
                    v2_initialize_request(ProtocolVersion::V2),
                )?)
                .expect_err("clients must not receive initialization requests");
            assert!(
                error
                    .data
                    .as_ref()
                    .and_then(|data| data.as_str())
                    .is_some_and(|data| data.contains("clients send `initialize`")),
                "{error:?}"
            );
            assert_eq!(
                initialization_state(&client),
                InitializationState::Uninitialized
            );

            client.outgoing_message(
                UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
                RemoteStyle::Counterpart,
            )?;
            client.incoming_response(
                "initialize",
                Ok(serde_json::to_value(v2_initialize_response(
                    ProtocolVersion::V2,
                ))?),
            )?;
            let error = client
                .outgoing_message(
                    UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V2))?,
                    RemoteStyle::Counterpart,
                )
                .expect_err("clients must reject reinitialization");
            assert!(
                error
                    .data
                    .as_ref()
                    .and_then(|data| data.as_str())
                    .is_some_and(|data| data.contains("only be initialized once")),
                "{error:?}"
            );
            assert_eq!(initialization_state(&client), InitializationState::Ready);
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
                Ok(serde_json::to_value(v2_initialize_response(
                    ProtocolVersion::V2,
                ))?),
            )?;
            assert_eq!(initialization_state(&compat), InitializationState::Ready);
            Ok(())
        }

        #[test]
        fn incoming_initialize_request_rejects_unsupported_protocol_version()
        -> Result<(), crate::Error> {
            let compat = ProtocolCompat::new(ProtocolMode::v2_agent());
            let error = compat
                .incoming_message(UntypedMessage::new(
                    "initialize",
                    v2_initialize_request(ProtocolVersion::V1),
                )?)
                .expect_err("v2 agents should reject v1 initialization without a v1 handler");
            let data = error
                .data
                .as_ref()
                .and_then(|data| data.as_str())
                .unwrap_or_default();
            assert!(
                data.contains("only supports ACP protocol version 2"),
                "{error:?}"
            );
            assert_eq!(negotiated(&compat), ProtocolVersionKind::V2);
            assert_eq!(compat.active_wire_version(), ProtocolVersionKind::V2);

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
