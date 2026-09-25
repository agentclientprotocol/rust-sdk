use agent_client_protocol::{
    Error, JsonRpcMessage, JsonRpcResponse, UntypedMessage,
    schema::{
        InitializeProxyRequest, METHOD_INITIALIZE_PROXY, ProtocolVersion,
        v1::{self, LoadSessionRequest, McpServer, NewSessionRequest, ResumeSessionRequest},
    },
};
use serde_json::{Map, Value};

#[cfg(feature = "unstable_session_fork")]
use agent_client_protocol::schema::v1::ForkSessionRequest;
#[cfg(feature = "unstable_protocol_v2")]
use agent_client_protocol::schema::v2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolyfillProtocol {
    V1,
    #[cfg(feature = "unstable_protocol_v2")]
    V2,
}

pub(super) enum NativeMcpOutcome {
    Result(Value),
    Error(Value),
}

impl PolyfillProtocol {
    /// Validate against the negotiated ACP version before projecting onto HTTP.
    pub(super) fn message_response(self, value: Value) -> Result<NativeMcpOutcome, Error> {
        match self {
            Self::V1 => match v1::MessageMcpResponse::from_value("mcp/message", value)? {
                v1::MessageMcpResponse::Result { result, .. } => {
                    Ok(NativeMcpOutcome::Result(result))
                }
                v1::MessageMcpResponse::Error { error, .. } => {
                    Ok(NativeMcpOutcome::Error(serde_json::to_value(error)?))
                }
                _ => Err(Error::invalid_request().data("unsupported MCP outcome")),
            },
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => match v2::MessageMcpResponse::from_value("mcp/message", value)? {
                v2::MessageMcpResponse::Result { result, .. } => {
                    Ok(NativeMcpOutcome::Result(result))
                }
                v2::MessageMcpResponse::Error { error, .. } => {
                    Ok(NativeMcpOutcome::Error(serde_json::to_value(error)?))
                }
                _ => Err(Error::invalid_request().data("unsupported MCP outcome")),
            },
        }
    }

    pub(crate) fn from_initialize_request(request: &UntypedMessage) -> Result<Self, Error> {
        if request.method() != METHOD_INITIALIZE_PROXY {
            return Err(Error::invalid_request().data("expected initialize proxy request"));
        }
        let requested = serde_json::from_value::<ProtocolVersion>(
            request
                .params()
                .get("protocolVersion")
                .cloned()
                .ok_or_else(|| {
                    Error::invalid_params().data("missing initialize.protocolVersion")
                })?,
        )
        .map_err(Error::into_internal_error)?;
        let protocol = if requested == ProtocolVersion::V1 {
            Self::V1
        } else {
            #[cfg(feature = "unstable_protocol_v2")]
            {
                if requested == ProtocolVersion::V2 {
                    Self::V2
                } else {
                    return Err(Error::invalid_request()
                        .data(format!("unsupported ACP protocol version {requested}")));
                }
            }
            #[cfg(not(feature = "unstable_protocol_v2"))]
            {
                return Err(Error::invalid_request()
                    .data(format!("unsupported ACP protocol version {requested}")));
            }
        };
        match protocol {
            Self::V1 => {
                InitializeProxyRequest::parse_message(request.method(), request.params())?;
            }
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => {
                v2::InitializeProxyRequest::parse_message(request.method(), request.params())?;
            }
        }
        Ok(protocol)
    }

    pub(crate) fn transform_initialize_response(
        self,
        response: &mut Value,
    ) -> Result<DownstreamMcpMode, Error> {
        let mode = match self {
            Self::V1 => {
                let parsed = agent_client_protocol::schema::v1::InitializeResponse::from_value(
                    "initialize",
                    response.clone(),
                )?;
                DownstreamMcpMode::from_capabilities(
                    parsed.agent_capabilities.mcp_capabilities.http,
                    parsed.agent_capabilities.mcp_capabilities.acp,
                )
            }
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => {
                let parsed = v2::InitializeResponse::from_value("initialize", response.clone())?;
                let mcp = parsed
                    .capabilities
                    .session
                    .as_ref()
                    .and_then(|s| s.mcp.as_ref());
                DownstreamMcpMode::from_capabilities(
                    mcp.is_some_and(|m| m.http.is_some()),
                    mcp.is_some_and(|m| m.acp.is_some()),
                )
            }
        };
        if mode == DownstreamMcpMode::HttpAdapter {
            let root = response.as_object_mut().ok_or_else(Error::invalid_params)?;
            match self {
                Self::V1 => {
                    let mcp = root
                        .get_mut("agentCapabilities")
                        .and_then(Value::as_object_mut)
                        .and_then(|capabilities| capabilities.get_mut("mcpCapabilities"))
                        .and_then(Value::as_object_mut)
                        .ok_or_else(Error::invalid_params)?;
                    mcp.insert("acp".into(), Value::Bool(true));
                }
                #[cfg(feature = "unstable_protocol_v2")]
                Self::V2 => {
                    let mcp = root
                        .get_mut("capabilities")
                        .and_then(Value::as_object_mut)
                        .and_then(|capabilities| capabilities.get_mut("session"))
                        .and_then(Value::as_object_mut)
                        .and_then(|session| session.get_mut("mcp"))
                        .and_then(Value::as_object_mut)
                        .ok_or_else(Error::invalid_params)?;
                    mcp.insert("acp".into(), Value::Object(Map::new()));
                }
            }
        }
        Ok(mode)
    }

    pub(crate) fn is_session_setup_method(self, method: &str) -> bool {
        match self {
            Self::V1 => {
                matches!(method, "session/new" | "session/load" | "session/resume")
                    || cfg!(feature = "unstable_session_fork") && method == "session/fork"
            }
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => {
                matches!(method, "session/new" | "session/resume")
                    || cfg!(feature = "unstable_session_fork") && method == "session/fork"
            }
        }
    }

    pub(crate) fn validate_session_setup_request(
        self,
        request: &UntypedMessage,
    ) -> Result<(), Error> {
        match self {
            Self::V1 => match request.method() {
                "session/new" => {
                    NewSessionRequest::parse_message(request.method(), request.params())?;
                }
                "session/load" => {
                    LoadSessionRequest::parse_message(request.method(), request.params())?;
                }
                "session/resume" => {
                    ResumeSessionRequest::parse_message(request.method(), request.params())?;
                }
                #[cfg(feature = "unstable_session_fork")]
                "session/fork" => {
                    ForkSessionRequest::parse_message(request.method(), request.params())?;
                }
                _ => return Err(Error::invalid_request().data("not a session setup method")),
            },
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => match request.method() {
                "session/new" => {
                    v2::NewSessionRequest::parse_message(request.method(), request.params())?;
                }
                "session/resume" => {
                    v2::ResumeSessionRequest::parse_message(request.method(), request.params())?;
                }
                #[cfg(feature = "unstable_session_fork")]
                "session/fork" => {
                    v2::ForkSessionRequest::parse_message(request.method(), request.params())?;
                }
                _ => return Err(Error::invalid_request().data("not a session setup method")),
            },
        }
        Ok(())
    }

    pub(crate) fn native_server(self, value: Value) -> Option<NativeServer> {
        let raw = value.as_object()?.clone();
        let (name, server_id) = match self {
            Self::V1 => {
                let McpServer::Acp(server) = serde_json::from_value(value).ok()? else {
                    return None;
                };
                (server.name, server.server_id.to_string())
            }
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => {
                let v2::McpServer::Acp(server) = serde_json::from_value(value).ok()? else {
                    return None;
                };
                (server.name, server.server_id.to_string())
            }
        };
        Some(NativeServer {
            raw,
            name,
            server_id,
        })
    }

    pub(crate) fn message_request(
        self,
        server_id: String,
        request_id: String,
        method: String,
        params: Option<Map<String, Value>>,
        meta: Option<Value>,
    ) -> Result<UntypedMessage, Error> {
        let mut wrapper = Map::new();
        wrapper.insert("serverId".into(), server_id.into());
        wrapper.insert("requestId".into(), request_id.into());
        wrapper.insert("method".into(), method.into());
        if let Some(params) = params {
            wrapper.insert("params".into(), Value::Object(params));
        }
        if let Some(meta) = meta {
            wrapper.insert("_meta".into(), meta);
        }
        let request = UntypedMessage {
            method: "mcp/message".into(),
            params: Value::Object(wrapper),
        };
        // Validate the selected schema without losing unknown wrapper fields.
        match self {
            Self::V1 => {
                agent_client_protocol::schema::v1::MessageMcpRequest::parse_message(
                    request.method(),
                    request.params(),
                )?;
            }
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => {
                v2::MessageMcpRequest::parse_message(request.method(), request.params())?;
            }
        }
        Ok(request)
    }

    pub(crate) fn parse_notification(
        self,
        raw: UntypedMessage,
    ) -> Result<NativeMcpNotification, Error> {
        let (server_id, request_id, method, params) = match self {
            Self::V1 => {
                let parsed =
                    agent_client_protocol::schema::v1::MessageMcpNotification::parse_message(
                        raw.method(),
                        raw.params(),
                    )?;
                (
                    parsed.server_id.to_string(),
                    parsed.request_id.to_string(),
                    parsed.method,
                    parsed.params,
                )
            }
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => {
                let parsed = v2::MessageMcpNotification::parse_message(raw.method(), raw.params())?;
                (
                    parsed.server_id.to_string(),
                    parsed.request_id.to_string(),
                    parsed.method,
                    parsed.params,
                )
            }
        };
        Ok(NativeMcpNotification {
            raw,
            server_id,
            request_id,
            method,
            params,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DownstreamMcpMode {
    #[default]
    Unknown,
    Native,
    HttpAdapter,
    Unavailable,
}

impl DownstreamMcpMode {
    pub(crate) fn from_capabilities(http: bool, acp: bool) -> Self {
        if acp {
            Self::Native
        } else if http {
            Self::HttpAdapter
        } else {
            Self::Unavailable
        }
    }
}

#[derive(Debug)]
pub(crate) struct NativeServer {
    raw: Map<String, Value>,
    pub(crate) name: String,
    pub(crate) server_id: String,
}

impl NativeServer {
    pub(crate) fn http_declaration(
        mut self,
        protocol: PolyfillProtocol,
        url: String,
        token: &str,
    ) -> Result<Value, Error> {
        self.raw.remove("serverId");
        self.raw.insert("type".into(), "http".into());
        self.raw.insert("name".into(), self.name.into());
        self.raw.insert("url".into(), url.into());
        self.raw.insert(
            "headers".into(),
            serde_json::json!([
                { "name": "Authorization", "value": format!("Bearer {token}") }
            ]),
        );
        let declaration = Value::Object(self.raw);
        match protocol {
            PolyfillProtocol::V1 => {
                serde_json::from_value::<McpServer>(declaration.clone())
                    .map_err(Error::into_internal_error)?;
            }
            #[cfg(feature = "unstable_protocol_v2")]
            PolyfillProtocol::V2 => {
                serde_json::from_value::<v2::McpServer>(declaration.clone())
                    .map_err(Error::into_internal_error)?;
            }
        }
        Ok(declaration)
    }
}

pub(crate) struct NativeMcpNotification {
    pub(crate) raw: UntypedMessage,
    pub(crate) server_id: String,
    pub(crate) request_id: String,
    pub(crate) method: String,
    pub(crate) params: Option<Map<String, Value>>,
}
