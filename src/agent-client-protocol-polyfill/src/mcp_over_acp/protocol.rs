use agent_client_protocol::{
    Error, JsonRpcMessage, JsonRpcResponse, UntypedMessage,
    schema::{
        InitializeProxyRequest, METHOD_INITIALIZE_PROXY, ProtocolVersion,
        v1::{
            ConnectMcpRequest, ConnectMcpResponse, DisconnectMcpRequest, DisconnectMcpResponse,
            LoadSessionRequest, McpServer, MessageMcpNotification, MessageMcpRequest,
            NewSessionRequest, ResumeSessionRequest,
        },
    },
};
use serde_json::{Map, Value};

#[cfg(feature = "unstable_protocol_v2")]
use agent_client_protocol::schema::v2;

#[cfg(feature = "unstable_session_fork")]
use agent_client_protocol::schema::v1::ForkSessionRequest;

/// ACP schema selected by the conductor's proxy initialization request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolyfillProtocol {
    V1,
    #[cfg(feature = "unstable_protocol_v2")]
    V2,
}

impl PolyfillProtocol {
    pub(crate) fn from_initialize_request(request: &UntypedMessage) -> Result<Self, Error> {
        if request.method() != METHOD_INITIALIZE_PROXY {
            return Err(Error::invalid_request()
                .data(format!("expected `{METHOD_INITIALIZE_PROXY}` request")));
        }

        let requested = request
            .params()
            .get("protocolVersion")
            .cloned()
            .ok_or_else(invalid_initialize_protocol_version)
            .and_then(|version| {
                serde_json::from_value::<ProtocolVersion>(version)
                    .map_err(|_| invalid_initialize_protocol_version())
            })?;

        let protocol = if requested == ProtocolVersion::V1 {
            Self::V1
        } else {
            #[cfg(feature = "unstable_protocol_v2")]
            {
                if requested == ProtocolVersion::V2 {
                    Self::V2
                } else {
                    return Err(unsupported_protocol_version(requested));
                }
            }

            #[cfg(not(feature = "unstable_protocol_v2"))]
            {
                return Err(unsupported_protocol_version(requested));
            }
        };

        protocol.validate_initialize_request(request)?;
        Ok(protocol)
    }

    fn validate_initialize_request(self, request: &UntypedMessage) -> Result<(), Error> {
        match self {
            Self::V1 => {
                InitializeProxyRequest::parse_message(request.method(), request.params())?;
            }
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => {
                v2::InitializeProxyRequest::parse_message(request.method(), request.params())?;
            }
        }
        Ok(())
    }

    pub(crate) fn transform_initialize_response(
        self,
        response: &mut Value,
    ) -> Result<DownstreamMcpMode, Error> {
        let mode = match self {
            Self::V1 => {
                let response = agent_client_protocol::schema::v1::InitializeResponse::from_value(
                    "initialize",
                    response.clone(),
                )?;
                DownstreamMcpMode::from_capabilities(
                    response.agent_capabilities.mcp_capabilities.http,
                    response.agent_capabilities.mcp_capabilities.acp,
                )
            }
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => {
                let response = v2::InitializeResponse::from_value("initialize", response.clone())?;
                let mcp = response
                    .capabilities
                    .session
                    .as_ref()
                    .and_then(|session| session.mcp.as_ref());
                DownstreamMcpMode::from_capabilities(
                    mcp.is_some_and(|mcp| mcp.http.is_some()),
                    mcp.is_some_and(|mcp| mcp.acp.is_some()),
                )
            }
        };

        if mode == DownstreamMcpMode::HttpAdapter {
            self.advertise_native_mcp(response)?;
        }
        Ok(mode)
    }

    fn advertise_native_mcp(self, response: &mut Value) -> Result<(), Error> {
        let response = response
            .as_object_mut()
            .ok_or_else(|| invalid_initialize_response("result must be an object"))?;
        match self {
            Self::V1 => {
                let mcp = response
                    .get_mut("agentCapabilities")
                    .and_then(Value::as_object_mut)
                    .and_then(|capabilities| capabilities.get_mut("mcpCapabilities"))
                    .and_then(Value::as_object_mut)
                    .ok_or_else(|| {
                        invalid_initialize_response(
                            "HTTP MCP support did not have an object capability container",
                        )
                    })?;
                mcp.insert("acp".into(), Value::Bool(true));
            }
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => {
                let mcp = response
                    .get_mut("capabilities")
                    .and_then(Value::as_object_mut)
                    .and_then(|capabilities| capabilities.get_mut("session"))
                    .and_then(Value::as_object_mut)
                    .and_then(|session| session.get_mut("mcp"))
                    .and_then(Value::as_object_mut)
                    .ok_or_else(|| {
                        invalid_initialize_response(
                            "HTTP MCP support did not have an object capability container",
                        )
                    })?;
                mcp.insert("acp".into(), Value::Object(Map::new()));
            }
        }
        Ok(())
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
                method => return Err(unexpected_session_setup_method(method)),
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
                method => return Err(unexpected_session_setup_method(method)),
            },
        }
        Ok(())
    }

    pub(crate) fn native_server(self, value: Value) -> Option<NativeServer> {
        let raw = value.as_object()?.clone();
        match self {
            Self::V1 => {
                let McpServer::Acp(server) = serde_json::from_value(value).ok()? else {
                    return None;
                };
                Some(NativeServer {
                    raw,
                    name: server.name,
                    server_id: server.server_id.to_string(),
                })
            }
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => {
                let v2::McpServer::Acp(server) = serde_json::from_value(value).ok()? else {
                    return None;
                };
                Some(NativeServer {
                    raw,
                    name: server.name,
                    server_id: server.server_id.to_string(),
                })
            }
        }
    }

    pub(crate) fn connect_request(self, server_id: String) -> Result<UntypedMessage, Error> {
        match self {
            Self::V1 => ConnectMcpRequest::new(server_id).to_untyped_message(),
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => v2::ConnectMcpRequest::new(server_id).to_untyped_message(),
        }
    }

    pub(crate) fn connect_response_id(self, response: Value) -> Result<String, Error> {
        match self {
            Self::V1 => ConnectMcpResponse::from_value("mcp/connect", response)
                .map(|response| response.connection_id.to_string()),
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => v2::ConnectMcpResponse::from_value("mcp/connect", response)
                .map(|response| response.connection_id.to_string()),
        }
    }

    pub(crate) fn message_request(
        self,
        connection_id: String,
        message: UntypedMessage,
    ) -> Result<UntypedMessage, Error> {
        let (method, params) = message.into_parts();
        let params = into_mcp_params(params)?;
        match self {
            Self::V1 => MessageMcpRequest::new(connection_id, method)
                .params(params)
                .to_untyped_message(),
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => v2::MessageMcpRequest::new(connection_id, method)
                .params(params)
                .to_untyped_message(),
        }
    }

    pub(crate) fn message_notification(
        self,
        connection_id: String,
        message: UntypedMessage,
    ) -> Result<UntypedMessage, Error> {
        let (method, params) = message.into_parts();
        let params = into_mcp_params(params)?;
        match self {
            Self::V1 => MessageMcpNotification::new(connection_id, method)
                .params(params)
                .to_untyped_message(),
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => v2::MessageMcpNotification::new(connection_id, method)
                .params(params)
                .to_untyped_message(),
        }
    }

    pub(crate) fn parse_message_request(
        self,
        request: UntypedMessage,
    ) -> Result<NativeMcpMessage, Error> {
        match self {
            Self::V1 => {
                let parsed = MessageMcpRequest::parse_message(request.method(), request.params())?;
                Ok(NativeMcpMessage {
                    raw: request,
                    connection_id: parsed.connection_id.to_string(),
                    method: parsed.method,
                    params: parsed.params,
                })
            }
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => {
                let parsed =
                    v2::MessageMcpRequest::parse_message(request.method(), request.params())?;
                Ok(NativeMcpMessage {
                    raw: request,
                    connection_id: parsed.connection_id.to_string(),
                    method: parsed.method,
                    params: parsed.params,
                })
            }
        }
    }

    pub(crate) fn parse_message_notification(
        self,
        notification: UntypedMessage,
    ) -> Result<NativeMcpMessage, Error> {
        match self {
            Self::V1 => {
                let parsed = MessageMcpNotification::parse_message(
                    notification.method(),
                    notification.params(),
                )?;
                Ok(NativeMcpMessage {
                    raw: notification,
                    connection_id: parsed.connection_id.to_string(),
                    method: parsed.method,
                    params: parsed.params,
                })
            }
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => {
                let parsed = v2::MessageMcpNotification::parse_message(
                    notification.method(),
                    notification.params(),
                )?;
                Ok(NativeMcpMessage {
                    raw: notification,
                    connection_id: parsed.connection_id.to_string(),
                    method: parsed.method,
                    params: parsed.params,
                })
            }
        }
    }

    pub(crate) fn disconnect_request(self, connection_id: String) -> Result<UntypedMessage, Error> {
        match self {
            Self::V1 => DisconnectMcpRequest::new(connection_id).to_untyped_message(),
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => v2::DisconnectMcpRequest::new(connection_id).to_untyped_message(),
        }
    }

    pub(crate) fn validate_disconnect_response(self, response: Value) -> Result<(), Error> {
        match self {
            Self::V1 => {
                DisconnectMcpResponse::from_value("mcp/disconnect", response)?;
            }
            #[cfg(feature = "unstable_protocol_v2")]
            Self::V2 => {
                v2::DisconnectMcpResponse::from_value("mcp/disconnect", response)?;
            }
        }
        Ok(())
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
    ) -> Result<Value, Error> {
        self.raw.remove("serverId");
        self.raw.insert("type".into(), Value::String("http".into()));
        self.raw.insert("name".into(), Value::String(self.name));
        self.raw.insert("url".into(), Value::String(url));
        // V1 requires the field and v2 accepts it. Keeping the explicit empty
        // list gives both versions one stable raw compatibility shape.
        self.raw.insert("headers".into(), Value::Array(Vec::new()));
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

#[derive(Debug)]
pub(crate) struct NativeMcpMessage {
    pub(crate) raw: UntypedMessage,
    pub(crate) connection_id: String,
    pub(crate) method: String,
    pub(crate) params: Option<Map<String, Value>>,
}

pub(crate) fn native_params_into_value(params: Option<Map<String, Value>>) -> Value {
    params.map_or(Value::Null, Value::Object)
}

fn into_mcp_params(params: Value) -> Result<Option<Map<String, Value>>, Error> {
    match params {
        Value::Null => Ok(None),
        Value::Object(params) => Ok(Some(params)),
        params => Err(Error::invalid_params().data(serde_json::json!({
            "reason": "MCP message params must be an object or null",
            "params": params,
        }))),
    }
}

fn invalid_initialize_protocol_version() -> Error {
    Error::invalid_params().data("initialize.protocolVersion must be a valid ACP protocol version")
}

fn unsupported_protocol_version(version: ProtocolVersion) -> Error {
    Error::invalid_request().data(format!(
        "MCP-over-ACP polyfill does not support ACP protocol version {version}"
    ))
}

fn unexpected_session_setup_method(method: &str) -> Error {
    Error::invalid_request().data(format!(
        "`{method}` is not a session setup method for the selected ACP version"
    ))
}

fn invalid_initialize_response(reason: &'static str) -> Error {
    Error::invalid_params().data(format!("invalid initialize response: {reason}"))
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::{
        JsonRpcMessage,
        schema::{ProtocolVersion, v1},
    };

    #[cfg(feature = "unstable_protocol_v2")]
    use agent_client_protocol::{ErrorCode, JsonRpcResponse};

    use super::PolyfillProtocol;

    #[test]
    fn http_declaration_preserves_extension_fields() {
        let declaration = serde_json::json!({
            "type": "acp",
            "name": "native",
            "serverId": "native-id",
            "_meta": {
                "source": "test"
            },
            "futureField": {
                "preserve": true
            }
        });
        let native = PolyfillProtocol::V1
            .native_server(declaration)
            .expect("the declaration should be recognized as native MCP");

        let transformed = native
            .http_declaration(PolyfillProtocol::V1, "http://127.0.0.1:4321".to_string())
            .expect("the transformed declaration should be valid v1 MCP");

        assert_eq!(
            transformed,
            serde_json::json!({
                "type": "http",
                "name": "native",
                "url": "http://127.0.0.1:4321",
                "headers": [],
                "_meta": {
                    "source": "test"
                },
                "futureField": {
                    "preserve": true
                }
            })
        );
    }

    #[test]
    fn native_message_keeps_the_original_wrapper() {
        let request = agent_client_protocol::UntypedMessage {
            method: "mcp/message".to_string(),
            params: serde_json::json!({
                "connectionId": "connection",
                "method": "tools/list",
                "params": {
                    "cursor": "next"
                },
                "_meta": {
                    "trace": "preserve"
                },
                "futureField": true
            }),
        };

        let parsed = PolyfillProtocol::V1
            .parse_message_request(request.clone())
            .expect("the native wrapper should parse");

        assert_eq!(parsed.raw, request);
        assert_eq!(parsed.connection_id, "connection");
        assert_eq!(parsed.method, "tools/list");
        assert_eq!(
            parsed.params,
            Some(serde_json::Map::from_iter([(
                "cursor".to_string(),
                serde_json::json!("next")
            )]))
        );
    }

    #[test]
    fn v1_session_setup_methods_match_the_stable_schema() {
        assert!(PolyfillProtocol::V1.is_session_setup_method("session/new"));
        assert!(PolyfillProtocol::V1.is_session_setup_method("session/load"));
        assert!(PolyfillProtocol::V1.is_session_setup_method("session/resume"));
        assert_eq!(
            PolyfillProtocol::V1.is_session_setup_method("session/fork"),
            cfg!(feature = "unstable_session_fork")
        );
        assert!(!PolyfillProtocol::V1.is_session_setup_method("session/prompt"));
    }

    #[test]
    fn session_setup_validation_allows_extensions_but_rejects_invalid_fields() {
        let mut request = v1::NewSessionRequest::new(std::path::PathBuf::from("/tmp"))
            .to_untyped_message()
            .expect("the session request should serialize");
        request.params["futureField"] = serde_json::json!({
            "preserve": true
        });
        PolyfillProtocol::V1
            .validate_session_setup_request(&request)
            .expect("extension fields should remain forward-compatible");

        request.params["cwd"] = serde_json::json!(42);
        let error = PolyfillProtocol::V1
            .validate_session_setup_request(&request)
            .expect_err("invalid selected-schema fields must be rejected");
        assert_eq!(error.code, agent_client_protocol::ErrorCode::InvalidParams);
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn v2_session_setup_methods_exclude_v1_load() {
        assert!(PolyfillProtocol::V2.is_session_setup_method("session/new"));
        assert!(!PolyfillProtocol::V2.is_session_setup_method("session/load"));
        assert!(PolyfillProtocol::V2.is_session_setup_method("session/resume"));
        assert_eq!(
            PolyfillProtocol::V2.is_session_setup_method("session/fork"),
            cfg!(feature = "unstable_session_fork")
        );
        assert!(!PolyfillProtocol::V2.is_session_setup_method("session/prompt"));
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn future_protocol_version_is_not_assumed_to_be_v2() {
        let initialize = agent_client_protocol::schema::v2::InitializeRequest::new(
            ProtocolVersion::V2,
            agent_client_protocol::schema::v2::Implementation::new("test", "1.0.0"),
        );
        let mut request =
            agent_client_protocol::schema::v2::InitializeProxyRequest::new(initialize)
                .to_untyped_message()
                .expect("the initialize request should serialize");
        request.params["protocolVersion"] = serde_json::json!(3);

        let error = PolyfillProtocol::from_initialize_request(&request)
            .expect_err("an unselected future schema must not be interpreted as v2");

        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(
            error.data,
            Some(serde_json::json!(
                "MCP-over-ACP polyfill does not support ACP protocol version 3"
            ))
        );
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn v2_initialize_adaptation_preserves_the_raw_response() {
        use agent_client_protocol::schema::v2;

        let response = v2::InitializeResponse::new(
            ProtocolVersion::V2,
            v2::Implementation::new("test", "1.0.0"),
        )
        .capabilities(
            v2::AgentCapabilities::new().session(
                v2::SessionCapabilities::new()
                    .mcp(v2::McpCapabilities::new().http(v2::McpHttpCapabilities::new())),
            ),
        );
        let mut response = serde_json::to_value(response).expect("the response should serialize");
        response["futureField"] = serde_json::json!({
            "preserve": true
        });

        let mode = PolyfillProtocol::V2
            .transform_initialize_response(&mut response)
            .expect("the v2 HTTP capability should be adaptable");

        assert_eq!(mode, super::DownstreamMcpMode::HttpAdapter);
        assert_eq!(
            response["capabilities"]["session"]["mcp"]["acp"],
            serde_json::json!({})
        );
        assert_eq!(
            response["futureField"],
            serde_json::json!({
                "preserve": true
            })
        );

        v2::InitializeResponse::from_value("initialize", response)
            .expect("the adapted response should remain valid v2");
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn v2_disconnect_uses_and_validates_the_selected_schema() {
        use agent_client_protocol::schema::v2;

        let request = PolyfillProtocol::V2
            .disconnect_request("connection".to_string())
            .expect("the v2 disconnect request should serialize");
        let parsed = v2::DisconnectMcpRequest::parse_message(request.method(), request.params())
            .expect("the disconnect request should be valid v2");
        assert_eq!(parsed.connection_id.to_string(), "connection");

        let response = serde_json::to_value(v2::DisconnectMcpResponse::new())
            .expect("the v2 disconnect response should serialize");
        PolyfillProtocol::V2
            .validate_disconnect_response(response)
            .expect("the v2 disconnect response should validate");
    }

    #[test]
    fn v1_initialize_request_selects_v1() {
        let request = agent_client_protocol::schema::InitializeProxyRequest {
            initialize: v1::InitializeRequest::new(ProtocolVersion::V1),
        }
        .to_untyped_message()
        .expect("the initialize request should serialize");

        assert_eq!(
            PolyfillProtocol::from_initialize_request(&request)
                .expect("the request should select v1"),
            PolyfillProtocol::V1
        );
    }
}
