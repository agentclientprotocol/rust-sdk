#[cfg(not(feature = "unstable_protocol_v2"))]
mod imp {
    #![allow(clippy::unused_self, clippy::unnecessary_wraps)]
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
    }
}

#[cfg(feature = "unstable_protocol_v2")]
mod imp {
    use std::sync::{Arc, Mutex};

    use crate::schema::ProtocolVersion;
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
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum InitializeSurface {
        Peer,
        Proxy,
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
            })
        }

        pub(crate) fn v1_client() -> Self {
            Self::Acp(AcpProtocolMode {
                api: ProtocolVersionKind::V1,
                initialize_surface: InitializeSurface::Peer,
            })
        }

        pub(crate) fn v1_proxy() -> Self {
            Self::Acp(AcpProtocolMode {
                api: ProtocolVersionKind::V1,
                initialize_surface: InitializeSurface::Proxy,
            })
        }

        pub(crate) fn v2_agent() -> Self {
            Self::Acp(AcpProtocolMode {
                api: ProtocolVersionKind::V2,
                initialize_surface: InitializeSurface::Peer,
            })
        }

        pub(crate) fn v2_client() -> Self {
            Self::Acp(AcpProtocolMode {
                api: ProtocolVersionKind::V2,
                initialize_surface: InitializeSurface::Peer,
            })
        }

        pub(crate) fn v2_proxy() -> Self {
            Self::Acp(AcpProtocolMode {
                api: ProtocolVersionKind::V2,
                initialize_surface: InitializeSurface::Proxy,
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

            Self {
                mode,
                state: Arc::new(Mutex::new(ProtocolState {
                    negotiated,
                    pending_initialize: None,
                })),
            }
        }

        pub(crate) fn incoming_message(
            &self,
            message: UntypedMessage,
        ) -> Result<UntypedMessage, crate::Error> {
            let Some(mode) = self.mode else {
                return Ok(message);
            };

            if mode.is_incoming_initialize_request(message.method()) {
                return self.incoming_initialize_request(mode, message);
            }
            if mode.initialize_surface == InitializeSurface::Proxy
                && (message.method() == "initialize" || successor_encloses_initialize(&message))
            {
                return Err(invalid_proxy_initialize_direction());
            }

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
                self.set_pending_initialize(mode.api);
                mode.api
            } else {
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
            ensure_matching_protocol_version(method, self.active_wire_version(), mode.api)?;
            Ok(value)
        }

        pub(crate) fn outgoing_response(
            &self,
            method: &str,
            result: Result<serde_json::Value, crate::Error>,
        ) -> Result<serde_json::Value, crate::Error> {
            let Some(mode) = self.mode else {
                return result;
            };

            // Always drain any pending initialize state so a failed initialize
            // doesn't leak negotiation state to a subsequent request.
            let pending_initialize = if mode.is_outgoing_initialize_response(method) {
                self.take_pending_initialize()
            } else {
                None
            };

            let mut value = result?;

            let wire_version = if mode.is_outgoing_initialize_response(method) {
                let negotiated = pending_initialize.unwrap_or(mode.api);
                ensure_matching_protocol_version(method, mode.api, negotiated)?;
                set_protocol_version(&mut value, negotiated)?;
                self.set_negotiated(negotiated);
                negotiated
            } else {
                self.active_wire_version()
            };

            ensure_matching_protocol_version(method, mode.api, wire_version)?;
            Ok(value)
        }

        fn incoming_initialize_request(
            &self,
            mode: AcpProtocolMode,
            mut message: UntypedMessage,
        ) -> Result<UntypedMessage, crate::Error> {
            let requested = required_protocol_version_from_value(message.params())?;
            let requested_kind = ProtocolVersionKind::from_protocol_version(requested)
                .ok_or_else(|| unsupported_protocol_version(requested, mode.api))?;
            if requested_kind != mode.api {
                return Err(unsupported_protocol_version(requested, mode.api));
            }

            self.set_pending_initialize(mode.api);
            set_protocol_version(&mut message.params, mode.api)?;
            Ok(message)
        }

        fn incoming_initialize_response(
            &self,
            mode: AcpProtocolMode,
            result: Result<serde_json::Value, crate::Error>,
        ) -> Result<serde_json::Value, crate::Error> {
            let _pending_initialize = self.take_pending_initialize();
            let mut value = result?;
            let response_version = required_protocol_version_from_value(&value)?;
            let wire_version = ProtocolVersionKind::from_protocol_version(response_version)
                .ok_or_else(|| unsupported_protocol_version(response_version, mode.api))?;
            if wire_version != mode.api {
                return Err(required_protocol_version(mode.api, wire_version));
            }
            self.set_negotiated(wire_version);

            set_protocol_version(&mut value, wire_version)?;
            Ok(value)
        }

        fn active_wire_version(&self) -> ProtocolVersionKind {
            let state = self
                .state
                .lock()
                .expect("protocol compatibility state mutex poisoned");
            state.pending_initialize.unwrap_or(state.negotiated)
        }

        fn set_negotiated(&self, negotiated: ProtocolVersionKind) {
            self.state
                .lock()
                .expect("protocol compatibility state mutex poisoned")
                .negotiated = negotiated;
        }

        fn set_pending_initialize(&self, negotiated: ProtocolVersionKind) {
            self.state
                .lock()
                .expect("protocol compatibility state mutex poisoned")
                .pending_initialize = Some(negotiated);
        }

        fn take_pending_initialize(&self) -> Option<ProtocolVersionKind> {
            self.state
                .lock()
                .expect("protocol compatibility state mutex poisoned")
                .pending_initialize
                .take()
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

            compat.incoming_message(UntypedMessage::new(
                "initialize",
                v2_initialize_request(ProtocolVersion::V2),
            )?)?;

            assert_eq!(negotiated(&compat), ProtocolVersionKind::V2);
            assert_eq!(compat.active_wire_version(), ProtocolVersionKind::V2);

            compat.outgoing_response(
                "initialize",
                Ok(serde_json::to_value(v2_initialize_response(
                    ProtocolVersion::V2,
                ))?),
            )?;

            assert_eq!(negotiated(&compat), ProtocolVersionKind::V2);
            assert_eq!(compat.active_wire_version(), ProtocolVersionKind::V2);
            Ok(())
        }

        #[test]
        fn outgoing_initialize_sets_active_wire_version_before_response() -> Result<(), crate::Error>
        {
            let compat = ProtocolCompat::new(ProtocolMode::v2_client());
            assert_eq!(compat.active_wire_version(), ProtocolVersionKind::V2);

            compat.outgoing_message(
                UntypedMessage::new("initialize", v2_initialize_request(ProtocolVersion::V1))?,
                RemoteStyle::Counterpart,
            )?;

            assert_eq!(negotiated(&compat), ProtocolVersionKind::V2);
            assert_eq!(compat.active_wire_version(), ProtocolVersionKind::V2);

            compat.incoming_response(
                "initialize",
                Ok(serde_json::to_value(v2_initialize_response(
                    ProtocolVersion::V2,
                ))?),
            )?;

            assert_eq!(negotiated(&compat), ProtocolVersionKind::V2);
            assert_eq!(compat.active_wire_version(), ProtocolVersionKind::V2);
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
