use std::{marker::PhantomData, sync::Arc};

use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use rustc_hash::FxHashMap;
use serde_json::{Map, Value};

use crate::mcp_server::{McpConnectionContext, McpConnectionTo, McpServerConnect};
use crate::role;
use crate::role::HasPeer;
use crate::schema::v1::{
    ConnectMcpRequest, ConnectMcpResponse, DisconnectMcpRequest, DisconnectMcpResponse,
    McpConnectionId, McpServerAcpId, MessageMcpNotification, MessageMcpRequest, MessageMcpResponse,
};
use crate::util::MatchDispatchFrom;
use crate::{
    Agent, Channel, ConnectTo, ConnectionTo, Dispatch, HandleDispatchFrom, Handled,
    JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, Responder, Role, UntypedMessage,
};

/// Stable protocol v1 native MCP-over-ACP wire types.
pub(super) struct V1McpProtocol;

/// Draft protocol v2 native MCP-over-ACP wire types.
#[cfg(feature = "unstable_protocol_v2")]
pub(super) struct V2McpProtocol;

pub(super) struct McpMessage {
    method: String,
    params: Option<Map<String, Value>>,
}

pub(super) trait McpProtocol: Send + 'static {
    type ConnectRequest: JsonRpcRequest<Response = Self::ConnectResponse>;
    type ConnectResponse: JsonRpcResponse;
    type MessageRequest: JsonRpcRequest<Response = Self::MessageResponse>;
    type MessageNotification: JsonRpcNotification;
    type MessageResponse: JsonRpcResponse;
    type DisconnectRequest: JsonRpcRequest<Response = Self::DisconnectResponse>;
    type DisconnectResponse: JsonRpcResponse;

    fn connect_server_id(request: &Self::ConnectRequest) -> McpServerAcpId;
    fn connect_response(connection_id: McpConnectionId) -> Self::ConnectResponse;
    fn message_request(
        connection_id: McpConnectionId,
        method: String,
        params: Option<Map<String, Value>>,
    ) -> Self::MessageRequest;
    fn message_notification(
        connection_id: McpConnectionId,
        method: String,
        params: Option<Map<String, Value>>,
    ) -> Self::MessageNotification;
    fn message_request_connection_id(request: &Self::MessageRequest) -> McpConnectionId;
    fn message_notification_connection_id(
        notification: &Self::MessageNotification,
    ) -> McpConnectionId;
    fn into_message_request(request: Self::MessageRequest) -> McpMessage;
    fn into_message_notification(notification: Self::MessageNotification) -> McpMessage;
    fn disconnect_connection_id(request: &Self::DisconnectRequest) -> McpConnectionId;
    fn disconnect_response() -> Self::DisconnectResponse;
}

impl McpProtocol for V1McpProtocol {
    type ConnectRequest = ConnectMcpRequest;
    type ConnectResponse = ConnectMcpResponse;
    type MessageRequest = MessageMcpRequest;
    type MessageNotification = MessageMcpNotification;
    type MessageResponse = MessageMcpResponse;
    type DisconnectRequest = DisconnectMcpRequest;
    type DisconnectResponse = DisconnectMcpResponse;

    fn connect_server_id(request: &Self::ConnectRequest) -> McpServerAcpId {
        request.server_id.clone()
    }

    fn connect_response(connection_id: McpConnectionId) -> Self::ConnectResponse {
        ConnectMcpResponse::new(connection_id)
    }

    fn message_request(
        connection_id: McpConnectionId,
        method: String,
        params: Option<Map<String, Value>>,
    ) -> Self::MessageRequest {
        MessageMcpRequest::new(connection_id, method).params(params)
    }

    fn message_notification(
        connection_id: McpConnectionId,
        method: String,
        params: Option<Map<String, Value>>,
    ) -> Self::MessageNotification {
        MessageMcpNotification::new(connection_id, method).params(params)
    }

    fn message_request_connection_id(request: &Self::MessageRequest) -> McpConnectionId {
        request.connection_id.clone()
    }

    fn message_notification_connection_id(
        notification: &Self::MessageNotification,
    ) -> McpConnectionId {
        notification.connection_id.clone()
    }

    fn into_message_request(request: Self::MessageRequest) -> McpMessage {
        McpMessage {
            method: request.method,
            params: request.params,
        }
    }

    fn into_message_notification(notification: Self::MessageNotification) -> McpMessage {
        McpMessage {
            method: notification.method,
            params: notification.params,
        }
    }

    fn disconnect_connection_id(request: &Self::DisconnectRequest) -> McpConnectionId {
        request.connection_id.clone()
    }

    fn disconnect_response() -> Self::DisconnectResponse {
        DisconnectMcpResponse::new()
    }
}

#[cfg(feature = "unstable_protocol_v2")]
impl McpProtocol for V2McpProtocol {
    type ConnectRequest = crate::schema::v2::ConnectMcpRequest;
    type ConnectResponse = crate::schema::v2::ConnectMcpResponse;
    type MessageRequest = crate::schema::v2::MessageMcpRequest;
    type MessageNotification = crate::schema::v2::MessageMcpNotification;
    type MessageResponse = crate::schema::v2::MessageMcpResponse;
    type DisconnectRequest = crate::schema::v2::DisconnectMcpRequest;
    type DisconnectResponse = crate::schema::v2::DisconnectMcpResponse;

    fn connect_server_id(request: &Self::ConnectRequest) -> McpServerAcpId {
        McpServerAcpId::new(request.server_id.0.clone())
    }

    fn connect_response(connection_id: McpConnectionId) -> Self::ConnectResponse {
        crate::schema::v2::ConnectMcpResponse::new(connection_id.0)
    }

    fn message_request(
        connection_id: McpConnectionId,
        method: String,
        params: Option<Map<String, Value>>,
    ) -> Self::MessageRequest {
        crate::schema::v2::MessageMcpRequest::new(connection_id.0, method).params(params)
    }

    fn message_notification(
        connection_id: McpConnectionId,
        method: String,
        params: Option<Map<String, Value>>,
    ) -> Self::MessageNotification {
        crate::schema::v2::MessageMcpNotification::new(connection_id.0, method).params(params)
    }

    fn message_request_connection_id(request: &Self::MessageRequest) -> McpConnectionId {
        McpConnectionId::new(request.connection_id.0.clone())
    }

    fn message_notification_connection_id(
        notification: &Self::MessageNotification,
    ) -> McpConnectionId {
        McpConnectionId::new(notification.connection_id.0.clone())
    }

    fn into_message_request(request: Self::MessageRequest) -> McpMessage {
        McpMessage {
            method: request.method,
            params: request.params,
        }
    }

    fn into_message_notification(notification: Self::MessageNotification) -> McpMessage {
        McpMessage {
            method: notification.method,
            params: notification.params,
        }
    }

    fn disconnect_connection_id(request: &Self::DisconnectRequest) -> McpConnectionId {
        McpConnectionId::new(request.connection_id.0.clone())
    }

    fn disconnect_response() -> Self::DisconnectResponse {
        crate::schema::v2::DisconnectMcpResponse::new()
    }
}

/// The message handler for an MCP server offered to a particular session.
/// This is added as a dynamic handler to the connection context and handles
/// native MCP-over-ACP messages for the declared server ID.
pub(super) struct McpActiveSession<Counterpart: Role, Protocol = V1McpProtocol> {
    /// The opaque ACP transport identifier for this MCP server.
    server_id: McpServerAcpId,

    /// The MCP server we are managing.
    mcp_connect: Arc<dyn McpServerConnect<Counterpart>>,

    /// Active connections to MCP server tasks.
    connections: FxHashMap<McpConnectionId, mpsc::Sender<Dispatch>>,

    protocol: PhantomData<fn() -> Protocol>,
}

impl<Counterpart: Role, Protocol> McpActiveSession<Counterpart, Protocol>
where
    Counterpart: HasPeer<Agent>,
    Protocol: McpProtocol,
{
    pub fn new(
        server_id: McpServerAcpId,
        mcp_connect: Arc<dyn McpServerConnect<Counterpart>>,
    ) -> Self {
        Self {
            server_id,
            mcp_connect,
            connections: FxHashMap::default(),
            protocol: PhantomData,
        }
    }

    /// Handle a connection request for our MCP server by creating a new MCP connection.
    fn handle_connect_request(
        &mut self,
        request: Protocol::ConnectRequest,
        responder: Responder<Protocol::ConnectResponse>,
        acp_connection: &ConnectionTo<Counterpart>,
    ) -> Result<
        Handled<(
            Protocol::ConnectRequest,
            Responder<Protocol::ConnectResponse>,
        )>,
        crate::Error,
    > {
        let server_id = Protocol::connect_server_id(&request);
        if server_id != self.server_id {
            return Ok(Handled::No {
                message: (request, responder),
                retry: false,
            });
        }

        let connection_id =
            McpConnectionId::new(format!("mcp-over-acp-connection:{}", uuid::Uuid::new_v4()));
        let (mcp_server_tx, mut mcp_server_rx) = mpsc::channel(128);
        self.connections
            .insert(connection_id.clone(), mcp_server_tx);

        let (client_channel, server_channel) = Channel::duplex();

        let client_component = {
            let connection_id = connection_id.clone();
            let acp_connection = acp_connection.clone();

            role::mcp::Client
                .builder()
                .on_receive_dispatch(
                    async move |message: Dispatch, _mcp_connection| match message {
                        Dispatch::Request(request, responder) => {
                            let (method, params) = request.into_parts();
                            let params = match into_native_params(params) {
                                Ok(params) => params,
                                Err(error) => return responder.respond_with_error(error),
                            };
                            let request =
                                Protocol::message_request(connection_id.clone(), method, params);
                            let responder = responder.wrap_params(|method, result| {
                                result.and_then(|response: Protocol::MessageResponse| {
                                    response.into_json(method)
                                })
                            });
                            let message: Dispatch<
                                Protocol::MessageRequest,
                                Protocol::MessageNotification,
                            > = Dispatch::Request(request, responder);
                            acp_connection.send_proxied_message_to(Agent, message)
                        }
                        Dispatch::Notification(notification) => {
                            let (method, params) = notification.into_parts();
                            let params = match into_native_params(params) {
                                Ok(params) => params,
                                Err(error) => {
                                    tracing::warn!(
                                        ?error,
                                        "ignoring MCP notification with positional parameters"
                                    );
                                    return Ok(());
                                }
                            };
                            let notification = Protocol::message_notification(
                                connection_id.clone(),
                                method,
                                params,
                            );
                            let message: Dispatch<
                                Protocol::MessageRequest,
                                Protocol::MessageNotification,
                            > = Dispatch::Notification(notification);
                            acp_connection.send_proxied_message_to(Agent, message)
                        }
                        Dispatch::Response(result, router) => router.route_with_result(result),
                    },
                    crate::on_receive_dispatch!(),
                )
                .with_spawned(move |mcp_connection| async move {
                    // These messages were sent by the ACP agent. Forward them to the MCP server.
                    while let Some(message) = mcp_server_rx.next().await {
                        mcp_connection.send_proxied_message_to(role::mcp::Server, message)?;
                    }
                    Ok(())
                })
        };

        let spawned_server = self.mcp_connect.connect(McpConnectionTo {
            context: McpConnectionContext::Acp {
                server_id,
                connection_id: connection_id.clone(),
            },
            connection: acp_connection.clone(),
        });

        let spawn_results = acp_connection
            .spawn(async move { client_component.connect_to(client_channel).await })
            .and_then(|()| {
                acp_connection.spawn(async move { spawned_server.connect_to(server_channel).await })
            });

        match spawn_results {
            Ok(()) => {
                responder.respond(Protocol::connect_response(connection_id))?;
                Ok(Handled::Yes)
            }
            Err(error) => {
                self.connections.remove(&connection_id);
                responder.respond_with_error(error)?;
                Ok(Handled::Yes)
            }
        }
    }

    /// Forward a native MCP-over-ACP request to its MCP connection.
    async fn handle_mcp_over_acp_request(
        &mut self,
        request: Protocol::MessageRequest,
        responder: Responder<Protocol::MessageResponse>,
    ) -> Result<
        Handled<(
            Protocol::MessageRequest,
            Responder<Protocol::MessageResponse>,
        )>,
        crate::Error,
    > {
        let connection_id = Protocol::message_request_connection_id(&request);
        let Some(mcp_server_tx) = self.connections.get_mut(&connection_id) else {
            return Ok(Handled::No {
                message: (request, responder),
                retry: false,
            });
        };
        let message = Protocol::into_message_request(request);

        let untyped = UntypedMessage {
            method: message.method,
            params: native_params_into_value(message.params),
        };
        let responder = responder.wrap_params(|method, result| {
            result
                .and_then(|response: Value| Protocol::MessageResponse::from_value(method, response))
        });
        mcp_server_tx
            .send(Dispatch::Request(untyped, responder))
            .await
            .map_err(crate::Error::into_internal_error)?;

        Ok(Handled::Yes)
    }

    /// Forward a native MCP-over-ACP notification to its MCP connection.
    async fn handle_mcp_over_acp_notification(
        &mut self,
        notification: Protocol::MessageNotification,
    ) -> Result<Handled<Protocol::MessageNotification>, crate::Error> {
        let connection_id = Protocol::message_notification_connection_id(&notification);
        let Some(mcp_server_tx) = self.connections.get_mut(&connection_id) else {
            return Ok(Handled::No {
                message: notification,
                retry: false,
            });
        };
        let message = Protocol::into_message_notification(notification);

        let untyped = UntypedMessage {
            method: message.method,
            params: native_params_into_value(message.params),
        };
        mcp_server_tx
            .send(Dispatch::Notification(untyped))
            .await
            .map_err(crate::Error::into_internal_error)?;

        Ok(Handled::Yes)
    }

    /// Disconnect an active native MCP-over-ACP connection.
    fn handle_mcp_disconnect_request(
        &mut self,
        request: Protocol::DisconnectRequest,
        responder: Responder<Protocol::DisconnectResponse>,
    ) -> Result<
        Handled<(
            Protocol::DisconnectRequest,
            Responder<Protocol::DisconnectResponse>,
        )>,
        crate::Error,
    > {
        let connection_id = Protocol::disconnect_connection_id(&request);
        if self.connections.remove(&connection_id).is_none() {
            return Ok(Handled::No {
                message: (request, responder),
                retry: false,
            });
        }

        responder.respond(Protocol::disconnect_response())?;
        Ok(Handled::Yes)
    }
}

impl<Counterpart: Role, Protocol> HandleDispatchFrom<Counterpart>
    for McpActiveSession<Counterpart, Protocol>
where
    Counterpart: HasPeer<Agent>,
    Protocol: McpProtocol,
{
    fn describe_chain(&self) -> impl std::fmt::Debug {
        "McpServerSession"
    }

    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        connection: ConnectionTo<Counterpart>,
    ) -> Result<Handled<Dispatch>, crate::Error> {
        MatchDispatchFrom::new(message, &connection)
            .if_request_from(
                Agent,
                async |request: Protocol::ConnectRequest, responder| {
                    self.handle_connect_request(request, responder, &connection)
                },
            )
            .await
            .if_request_from(
                Agent,
                async |request: Protocol::MessageRequest, responder| {
                    self.handle_mcp_over_acp_request(request, responder).await
                },
            )
            .await
            .if_notification_from(
                Agent,
                async |notification: Protocol::MessageNotification| {
                    self.handle_mcp_over_acp_notification(notification).await
                },
            )
            .await
            .if_request_from(
                Agent,
                async |request: Protocol::DisconnectRequest, responder| {
                    self.handle_mcp_disconnect_request(request, responder)
                },
            )
            .await
            .done()
    }
}

fn into_native_params(params: Value) -> Result<Option<Map<String, Value>>, crate::Error> {
    match params {
        Value::Null => Ok(None),
        Value::Object(params) => Ok(Some(params)),
        Value::Array(_) => Err(crate::Error::invalid_params()
            .data("MCP-over-ACP only supports named inner MCP parameters")),
        _ => {
            Err(crate::Error::invalid_params()
                .data("inner MCP parameters must be an object or null"))
        }
    }
}

fn native_params_into_value(params: Option<Map<String, Value>>) -> Value {
    params.map_or(Value::Null, Value::Object)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{into_native_params, native_params_into_value};

    #[test]
    fn native_mcp_params_round_trip_objects_and_null() {
        let object = json!({ "name": "echo", "arguments": {} });
        let params = into_native_params(object.clone()).expect("object params should be valid");
        assert_eq!(native_params_into_value(params), object);

        let params = into_native_params(serde_json::Value::Null)
            .expect("omitted params should be represented as null");
        assert_eq!(native_params_into_value(params), serde_json::Value::Null);
    }

    #[test]
    fn native_mcp_params_reject_positional_params() {
        let error = into_native_params(json!(["positional"]))
            .expect_err("native MCP-over-ACP cannot represent positional params");
        assert_eq!(error.code, crate::ErrorCode::InvalidParams);
    }
}
