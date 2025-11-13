use sacp;
use sacp::{
    JrMessage, JrNotification, JrRequest, JrResponsePayload, UntypedMessage, util::json_cast,
};
use serde::{Deserialize, Serialize};

/// JSON-RPC method name for MCP connect requests
pub const METHOD_MCP_CONNECT_REQUEST: &str = "_mcp/connect";

/// Creates a new MCP connection. This is equivalent to "running the command".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConnectRequest {
    /// The ACP URL to connect to (e.g., "acp:uuid")
    pub acp_url: String,
}

impl JrMessage for McpConnectRequest {
    fn into_untyped_message(self) -> Result<UntypedMessage, sacp::Error> {
        UntypedMessage::new(METHOD_MCP_CONNECT_REQUEST, self)
    }

    fn method(&self) -> &str {
        METHOD_MCP_CONNECT_REQUEST
    }

    fn parse_request(method: &str, params: &impl Serialize) -> Option<Result<Self, sacp::Error>> {
        if method != METHOD_MCP_CONNECT_REQUEST {
            return None;
        }
        Some(sacp::util::json_cast(params))
    }

    fn parse_notification(
        _method: &str,
        _params: &impl Serialize,
    ) -> Option<Result<Self, sacp::Error>> {
        // This is a request, not a notification
        None
    }
}

impl JrRequest for McpConnectRequest {
    type Response = McpConnectResponse;
}

/// Response to an MCP connect request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConnectResponse {
    /// Unique identifier for the established MCP connection
    pub connection_id: String,
}

impl JrResponsePayload for McpConnectResponse {
    fn into_json(self, _method: &str) -> Result<serde_json::Value, sacp::Error> {
        serde_json::to_value(self).map_err(sacp::Error::into_internal_error)
    }

    fn from_value(_method: &str, value: serde_json::Value) -> Result<Self, sacp::Error> {
        serde_json::from_value(value).map_err(|_| sacp::Error::invalid_params())
    }
}

/// JSON-RPC method name for MCP disconnect notifications
pub const METHOD_MCP_DISCONNECT_NOTIFICATION: &str = "_mcp/disconnect";

/// Disconnects the MCP connection.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct McpDisconnectNotification {
    /// The id of the connection to disconnect.
    pub connection_id: String,
}

impl JrMessage for McpDisconnectNotification {
    fn into_untyped_message(self) -> Result<UntypedMessage, sacp::Error> {
        UntypedMessage::new(METHOD_MCP_DISCONNECT_NOTIFICATION, self)
    }

    fn method(&self) -> &str {
        METHOD_MCP_DISCONNECT_NOTIFICATION
    }

    fn parse_request(_method: &str, _params: &impl Serialize) -> Option<Result<Self, sacp::Error>> {
        // This is a notification, not a request
        None
    }

    fn parse_notification(
        method: &str,
        params: &impl Serialize,
    ) -> Option<Result<Self, sacp::Error>> {
        if method != METHOD_MCP_DISCONNECT_NOTIFICATION {
            return None;
        }
        Some(sacp::util::json_cast(params))
    }
}

impl JrNotification for McpDisconnectNotification {}

/// JSON-RPC method name for MCP requests over ACP
pub const METHOD_MCP_REQUEST: &str = "_mcp/request";

/// An MCP request sent via ACP. This could be an MCP-server-to-MCP-client request
/// (in which case it goes from the ACP client to the ACP agent,
/// note the reversal of roles) or an MCP-client-to-MCP-server request
/// (in which case it goes from the ACP agent to the ACP client).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpOverAcpRequest<R> {
    /// id given in response to `_mcp/connect` request.
    pub connection_id: String,

    /// Request to be sent to the MCP server or client.
    #[serde(flatten)]
    pub request: R,
}

impl<R: JrRequest> JrMessage for McpOverAcpRequest<R> {
    fn into_untyped_message(self) -> Result<UntypedMessage, sacp::Error> {
        let message = self.request.into_untyped_message()?;
        UntypedMessage::new(
            METHOD_MCP_REQUEST,
            McpOverAcpRequest {
                connection_id: self.connection_id,
                request: message,
            },
        )
    }

    fn method(&self) -> &str {
        METHOD_MCP_REQUEST
    }

    fn parse_request(method: &str, params: &impl Serialize) -> Option<Result<Self, sacp::Error>> {
        if method == METHOD_MCP_REQUEST {
            match json_cast::<_, McpOverAcpRequest<UntypedMessage>>(params) {
                Ok(outer) => match R::parse_request(&outer.request.method, &outer.request.params) {
                    Some(Ok(request)) => Some(Ok(McpOverAcpRequest {
                        connection_id: outer.connection_id,
                        request,
                    })),
                    Some(Err(err)) => Some(Err(err)),
                    None => None,
                },
                Err(err) => Some(Err(err)),
            }
        } else {
            None
        }
    }

    fn parse_notification(
        _method: &str,
        _params: &impl Serialize,
    ) -> Option<Result<Self, sacp::Error>> {
        None // Request, not notification
    }
}

impl<R: JrRequest> JrRequest for McpOverAcpRequest<R> {
    type Response = R::Response;
}

/// JSON-RPC method name for MCP notifications over ACP
pub const METHOD_MCP_NOTIFICATION: &str = "_mcp/notification";

/// An MCP notification sent via ACP, either from the MCP client (the ACP agent)
/// or the MCP server (the ACP client).
///
/// Delivered via `_mcp/notification` when the MCP client (the ACP agent)
/// sends a notification to the MCP server (the ACP client).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpOverAcpNotification<R> {
    /// id given in response to `_mcp/connect` request.
    pub connection_id: String,

    /// Notification to be sent to the MCP server or client.
    #[serde(flatten)]
    pub notification: R,
}

impl<R: JrMessage> JrMessage for McpOverAcpNotification<R> {
    fn into_untyped_message(self) -> Result<UntypedMessage, sacp::Error> {
        let params = self.notification.into_untyped_message()?;
        UntypedMessage::new(
            METHOD_MCP_NOTIFICATION,
            McpOverAcpNotification {
                connection_id: self.connection_id,
                notification: params,
            },
        )
    }

    fn method(&self) -> &str {
        METHOD_MCP_NOTIFICATION
    }

    fn parse_request(_method: &str, _params: &impl Serialize) -> Option<Result<Self, sacp::Error>> {
        None // Notification, not request
    }

    fn parse_notification(
        method: &str,
        params: &impl Serialize,
    ) -> Option<Result<Self, sacp::Error>> {
        if method == METHOD_MCP_NOTIFICATION {
            match json_cast::<_, McpOverAcpNotification<UntypedMessage>>(params) {
                Ok(outer) => match R::parse_notification(
                    &outer.notification.method,
                    &outer.notification.params,
                ) {
                    Some(Ok(notification)) => Some(Ok(McpOverAcpNotification {
                        connection_id: outer.connection_id,
                        notification,
                    })),
                    Some(Err(err)) => Some(Err(err)),
                    None => None,
                },
                Err(err) => Some(Err(err)),
            }
        } else {
            None
        }
    }
}

impl<R: JrMessage> JrNotification for McpOverAcpNotification<R> {}
