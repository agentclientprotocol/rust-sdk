//! MCP-over-ACP compatibility proxy.
//!
//! This proxy adapts schema-native `McpServer::Acp` declarations for agents that do not
//! support the ACP MCP transport. It replaces those declarations with loopback HTTP bridges and
//! relays `mcp/connect`, `mcp/message`, and `mcp/disconnect` over ACP.
//!
//! Stable protocol v1 is supported by default. Enable the crate's
//! `unstable_protocol_v2` feature to use the same proxy in a draft-v2 conductor
//! chain.
//!
//! # Usage
//!
//! ```rust,ignore
//! use agent_client_protocol_polyfill::mcp_over_acp::McpOverAcpPolyfill;
//!
//! let conductor = ConductorImpl::new_agent(
//!     "conductor",
//!     ProxiesAndAgent::new(my_agent).proxy(McpOverAcpPolyfill::http()),
//! );
//! ```

mod actor;
pub(crate) mod http;
mod protocol;

use std::collections::HashMap;

use agent_client_protocol::{
    Agent, Client, Conductor, ConnectTo, ConnectionTo, Dispatch, HandleDispatchFrom, Handled,
    Proxy, Responder, UntypedMessage, is_cancel_request_notification, util::MatchDispatchFrom,
};
use futures::{SinkExt, channel::mpsc, channel::oneshot};
use serde_json::Value;
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

use self::actor::BridgeConnectionActor;
use self::protocol::{
    DownstreamMcpMode, NativeMcpMessage, NativeServer, PolyfillProtocol, native_params_into_value,
};

/// Internal messages for the polyfill's bridge management.
#[derive(Debug)]
pub(crate) enum BridgeMessage {
    /// Record the selected ACP schema and which MCP transport the successor can consume.
    SetProtocol {
        protocol: PolyfillProtocol,
        downstream_mode: DownstreamMcpMode,
    },

    /// Transform the MCP declarations for one session setup request.
    TransformServers {
        servers: Vec<Value>,
        response_tx: oneshot::Sender<Result<Vec<Value>, agent_client_protocol::Error>>,
    },

    /// A new TCP connection was accepted and needs a native MCP connection ID.
    ConnectionReceived {
        server_id: String,
        actor: BridgeConnectionActor,
        connection: BridgeConnection,
    },

    /// A native MCP connection ID was received; spawn the actor and store its sender.
    ConnectionEstablished {
        server_id: String,
        connection_id: String,
        actor: BridgeConnectionActor,
        connection: BridgeConnection,
    },

    /// Opening a native MCP connection failed.
    ConnectionFailed { server_id: String },

    /// An MCP message from the local agent that must be sent over ACP.
    ClientToServer {
        connection_id: String,
        message: Dispatch,
    },

    /// An MCP server request received over ACP for the local agent's MCP client.
    ServerToClientRequest {
        request: NativeMcpMessage,
        responder: Responder,
    },

    /// An MCP server notification received over ACP for the local agent's MCP client.
    ServerToClientNotification { notification: NativeMcpMessage },

    /// The local MCP bridge disconnected.
    Disconnected { connection_id: String },
}

/// Connection handle for sending messages to an MCP client via a bridge.
#[derive(Clone, Debug)]
pub(crate) struct BridgeConnection {
    to_mcp_client_tx: mpsc::Sender<Dispatch>,
}

impl BridgeConnection {
    pub fn new(to_mcp_client_tx: mpsc::Sender<Dispatch>) -> Self {
        Self { to_mcp_client_tx }
    }

    fn try_send(&mut self, message: Dispatch) -> Option<Box<Dispatch>> {
        self.to_mcp_client_tx
            .try_send(message)
            .err()
            .map(|error| Box::new(error.into_inner()))
    }
}

/// Adapts schema-native MCP-over-ACP declarations for agents that support HTTP MCP.
#[derive(Debug, Default)]
pub struct McpOverAcpPolyfill;

impl McpOverAcpPolyfill {
    /// Create a polyfill that exposes each ACP MCP server through loopback HTTP.
    #[must_use]
    pub fn http() -> Self {
        Self
    }
}

impl ConnectTo<Conductor> for McpOverAcpPolyfill {
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol::Error> {
        let (bridge_tx, bridge_rx) = mpsc::channel(128);

        let proxy = Proxy.builder();
        #[cfg(feature = "unstable_protocol_v2")]
        let proxy = proxy.without_acp_version_guard();

        proxy
            .name("mcp-over-acp-polyfill")
            .with_runner(BridgeRunner {
                bridge_tx: bridge_tx.clone(),
                bridge_rx,
                protocol: None,
                downstream_mode: DownstreamMcpMode::Unknown,
                listeners: BridgeListeners::default(),
                bridge_connections: HashMap::new(),
            })
            .with_handler(PolyfillHandler {
                protocol: None,
                bridge_tx,
            })
            .connect_to(client)
            .await
    }
}

#[derive(Debug)]
struct PolyfillHandler {
    protocol: Option<PolyfillProtocol>,
    bridge_tx: mpsc::Sender<BridgeMessage>,
}

impl HandleDispatchFrom<Conductor> for PolyfillHandler {
    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        cx: ConnectionTo<Conductor>,
    ) -> Result<Handled<Dispatch>, agent_client_protocol::Error> {
        MatchDispatchFrom::new(message, &cx)
            .if_dispatch_from(Client, async |message: Dispatch| {
                self.handle_client_dispatch(message, &cx).await
            })
            .await
            .done()
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        self
    }
}

impl PolyfillHandler {
    async fn handle_client_dispatch(
        &mut self,
        message: Dispatch,
        cx: &ConnectionTo<Conductor>,
    ) -> Result<Handled<Dispatch>, agent_client_protocol::Error> {
        match message {
            Dispatch::Request(request, responder) => {
                self.handle_client_request(request, responder, cx).await
            }
            Dispatch::Notification(notification) => {
                self.handle_client_notification(notification).await
            }
            message @ Dispatch::Response(_, _) => Ok(Handled::No {
                message,
                retry: false,
            }),
        }
    }

    async fn handle_client_request(
        &mut self,
        mut request: UntypedMessage,
        responder: Responder,
        cx: &ConnectionTo<Conductor>,
    ) -> Result<Handled<Dispatch>, agent_client_protocol::Error> {
        if request.method() == agent_client_protocol::schema::METHOD_INITIALIZE_PROXY {
            if self.protocol.is_some() {
                return Err(agent_client_protocol::Error::invalid_request()
                    .data("MCP-over-ACP polyfill was already initialized"));
            }
            let protocol = PolyfillProtocol::from_initialize_request(&request)?;
            self.protocol = Some(protocol);
            request.method = "initialize".to_string();

            let sent = cx.send_request_to(Agent, request);
            let sent = sent.forward_cancellation_from(responder.cancellation());
            let mut bridge_tx = self.bridge_tx.clone();
            sent.on_receiving_result(async move |result| {
                let result = match result {
                    Ok(response) => {
                        adapt_initialize_response(protocol, response, &mut bridge_tx).await
                    }
                    Err(error) => Err(error),
                };
                responder.respond_with_result(result)
            })?;
            return Ok(Handled::Yes);
        }

        let Some(protocol) = self.protocol else {
            return Ok(Handled::No {
                message: Dispatch::Request(request, responder),
                retry: false,
            });
        };

        if protocol.is_session_setup_method(request.method()) {
            protocol.validate_session_setup_request(&request)?;
            transform_session_servers(&mut request, &mut self.bridge_tx).await?;
            cx.send_request_to(Agent, request)
                .forward_response_to(responder)?;
            return Ok(Handled::Yes);
        }

        if request.method() == "mcp/message" {
            let request = protocol.parse_message_request(request)?;
            self.bridge_tx
                .send(BridgeMessage::ServerToClientRequest { request, responder })
                .await
                .map_err(agent_client_protocol::Error::into_internal_error)?;
            return Ok(Handled::Yes);
        }

        Ok(Handled::No {
            message: Dispatch::Request(request, responder),
            retry: false,
        })
    }

    async fn handle_client_notification(
        &mut self,
        notification: UntypedMessage,
    ) -> Result<Handled<Dispatch>, agent_client_protocol::Error> {
        let Some(protocol) = self.protocol else {
            return Ok(Handled::No {
                message: Dispatch::Notification(notification),
                retry: false,
            });
        };

        if notification.method() == "mcp/message" {
            let notification = protocol.parse_message_notification(notification)?;
            self.bridge_tx
                .send(BridgeMessage::ServerToClientNotification { notification })
                .await
                .map_err(agent_client_protocol::Error::into_internal_error)?;
            return Ok(Handled::Yes);
        }

        Ok(Handled::No {
            message: Dispatch::Notification(notification),
            retry: false,
        })
    }
}

async fn adapt_initialize_response(
    protocol: PolyfillProtocol,
    mut response: Value,
    bridge_tx: &mut mpsc::Sender<BridgeMessage>,
) -> Result<Value, agent_client_protocol::Error> {
    let downstream_mode = protocol.transform_initialize_response(&mut response)?;
    bridge_tx
        .send(BridgeMessage::SetProtocol {
            protocol,
            downstream_mode,
        })
        .await
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    Ok(response)
}

async fn transform_session_servers(
    request: &mut UntypedMessage,
    bridge_tx: &mut mpsc::Sender<BridgeMessage>,
) -> Result<(), agent_client_protocol::Error> {
    let Some(servers) = request
        .params
        .as_object_mut()
        .and_then(|params| params.get_mut("mcpServers"))
        .and_then(Value::as_array_mut)
    else {
        return Ok(());
    };

    let (response_tx, response_rx) = oneshot::channel();
    bridge_tx
        .send(BridgeMessage::TransformServers {
            servers: std::mem::take(servers),
            response_tx,
        })
        .await
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    *servers = response_rx
        .await
        .map_err(agent_client_protocol::Error::into_internal_error)??;
    Ok(())
}

#[derive(Default, Debug)]
struct BridgeListeners {
    listeners: HashMap<String, BridgeListener>,
}

#[derive(Clone, Debug)]
struct BridgeListener {
    tcp_port: u16,
}

impl BridgeListener {
    fn declaration(
        &self,
        protocol: PolyfillProtocol,
        server: NativeServer,
    ) -> Result<Value, agent_client_protocol::Error> {
        server.http_declaration(protocol, format!("http://127.0.0.1:{}", self.tcp_port))
    }
}

impl BridgeListeners {
    async fn transform_servers(
        &mut self,
        connection: &ConnectionTo<Conductor>,
        protocol: PolyfillProtocol,
        servers: Vec<Value>,
        bridge_tx: &mpsc::Sender<BridgeMessage>,
    ) -> Result<Vec<Value>, agent_client_protocol::Error> {
        let mut transformed = Vec::with_capacity(servers.len());
        for server in servers {
            transformed.push(
                self.transform_server(connection, protocol, server, bridge_tx)
                    .await?,
            );
        }
        Ok(transformed)
    }

    async fn transform_server(
        &mut self,
        connection: &ConnectionTo<Conductor>,
        protocol: PolyfillProtocol,
        server: Value,
        bridge_tx: &mpsc::Sender<BridgeMessage>,
    ) -> Result<Value, agent_client_protocol::Error> {
        let Some(native_server) = protocol.native_server(server.clone()) else {
            return Ok(server);
        };
        let server_id = native_server.server_id.clone();

        info!(
            server_name = %native_server.name,
            server_id,
            "detected native MCP-over-ACP server; creating compatibility bridge"
        );

        if let Some(listener) = self.listeners.get(&server_id) {
            return listener.declaration(protocol, native_server);
        }

        let tcp_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(agent_client_protocol::Error::into_internal_error)?;
        let tcp_port = tcp_listener
            .local_addr()
            .map_err(agent_client_protocol::Error::into_internal_error)?
            .port();
        let listener = BridgeListener { tcp_port };

        connection.spawn({
            let server_id = server_id.clone();
            let bridge_tx = bridge_tx.clone();
            async move {
                info!(
                    server_id,
                    tcp_port, "accepting MCP compatibility connections"
                );
                http::run_http_listener(tcp_listener, server_id, bridge_tx).await
            }
        })?;

        let declaration = listener.declaration(protocol, native_server)?;
        self.listeners.insert(server_id, listener);
        Ok(declaration)
    }

    fn remove(&mut self, server_id: &str) {
        self.listeners.remove(server_id);
    }
}

#[derive(Debug)]
struct ActiveBridgeConnection {
    server_id: String,
    bridge: BridgeConnection,
}

struct BridgeRunner {
    bridge_tx: mpsc::Sender<BridgeMessage>,
    bridge_rx: mpsc::Receiver<BridgeMessage>,
    protocol: Option<PolyfillProtocol>,
    downstream_mode: DownstreamMcpMode,
    listeners: BridgeListeners,
    bridge_connections: HashMap<String, ActiveBridgeConnection>,
}

impl std::fmt::Debug for BridgeRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeRunner")
            .field("protocol", &self.protocol)
            .field("downstream_mode", &self.downstream_mode)
            .field("listeners", &self.listeners.listeners.len())
            .field("bridge_connections", &self.bridge_connections.len())
            .finish_non_exhaustive()
    }
}

impl agent_client_protocol::RunWithConnectionTo<Conductor> for BridgeRunner {
    async fn run_with_connection_to(
        mut self,
        connection: ConnectionTo<Conductor>,
    ) -> Result<(), agent_client_protocol::Error> {
        use futures::StreamExt;

        while let Some(message) = self.bridge_rx.next().await {
            match message {
                BridgeMessage::SetProtocol {
                    protocol,
                    downstream_mode,
                } => {
                    self.protocol = Some(protocol);
                    self.downstream_mode = downstream_mode;
                }

                BridgeMessage::TransformServers {
                    servers,
                    response_tx,
                } => {
                    let result = match (self.protocol, self.downstream_mode) {
                        (Some(_), DownstreamMcpMode::Native) => Ok(servers),
                        (Some(protocol), DownstreamMcpMode::HttpAdapter) => {
                            self.listeners
                                .transform_servers(&connection, protocol, servers, &self.bridge_tx)
                                .await
                        }
                        (Some(protocol), DownstreamMcpMode::Unavailable) => reject_native_servers(
                            protocol,
                            servers,
                            "the downstream agent supports neither native nor HTTP MCP transport",
                        ),
                        (Some(protocol), DownstreamMcpMode::Unknown) => reject_native_servers(
                            protocol,
                            servers,
                            "MCP transport capabilities are unavailable before initialize",
                        ),
                        (None, _) => Err(agent_client_protocol::Error::invalid_request()
                            .data("MCP transport capabilities are unavailable before initialize")),
                    };
                    drop(response_tx.send(result));
                }

                BridgeMessage::ConnectionReceived {
                    server_id,
                    actor,
                    connection: bridge,
                } => {
                    let Some(protocol) = self.protocol else {
                        warn!(
                            server_id,
                            "cannot open MCP bridge before ACP initialization"
                        );
                        self.listeners.remove(&server_id);
                        continue;
                    };
                    let request = protocol.connect_request(server_id.clone())?;
                    let mut bridge_tx = self.bridge_tx.clone();
                    let scheduled = connection
                        .send_request_to(Client, request)
                        .on_receiving_result(async move |result| {
                            let message = match result {
                                Ok(response) => match protocol.connect_response_id(response) {
                                    Ok(connection_id) => BridgeMessage::ConnectionEstablished {
                                        server_id,
                                        connection_id,
                                        actor,
                                        connection: bridge,
                                    },
                                    Err(error) => {
                                        warn!(?error, "invalid response to mcp/connect");
                                        BridgeMessage::ConnectionFailed { server_id }
                                    }
                                },
                                Err(error) => {
                                    warn!(?error, "mcp/connect failed");
                                    BridgeMessage::ConnectionFailed { server_id }
                                }
                            };
                            drop(bridge_tx.send(message).await);
                            Ok(())
                        });
                    if let Err(error) = scheduled {
                        warn!(?error, "could not schedule mcp/connect response handling");
                    }
                }

                BridgeMessage::ConnectionEstablished {
                    server_id,
                    connection_id,
                    actor,
                    connection: bridge,
                } => {
                    self.bridge_connections.insert(
                        connection_id.clone(),
                        ActiveBridgeConnection { server_id, bridge },
                    );
                    connection.spawn(actor.run(connection_id))?;
                }

                BridgeMessage::ConnectionFailed { server_id } => {
                    self.listeners.remove(&server_id);
                }

                BridgeMessage::ClientToServer {
                    connection_id,
                    message,
                } => {
                    let Some(protocol) = self.protocol else {
                        let rejection = match message {
                            Dispatch::Request(_, responder) => responder
                                .respond_with_internal_error(
                                    "ACP protocol is unavailable before initialize",
                                ),
                            Dispatch::Notification(_) | Dispatch::Response(_, _) => Ok(()),
                        };
                        if let Err(error) = rejection {
                            debug!(?error, "could not reject MCP request before initialize");
                        }
                        continue;
                    };

                    match message {
                        Dispatch::Request(message, responder) => {
                            match protocol.message_request(connection_id, message) {
                                Ok(request) => {
                                    let pending = connection.send_request_to(Client, request);
                                    if let Err(error) = pending.forward_response_to(responder) {
                                        warn!(
                                            ?error,
                                            "could not forward local MCP request response"
                                        );
                                    }
                                }
                                Err(error) => {
                                    if let Err(send_error) = responder.respond_with_error(error) {
                                        debug!(
                                            ?send_error,
                                            "could not reject malformed MCP request"
                                        );
                                    }
                                }
                            }
                        }
                        Dispatch::Notification(message) => {
                            match local_mcp_notification(protocol, connection_id, message) {
                                Ok(Some(notification)) => {
                                    if let Err(error) =
                                        connection.send_notification_to(Client, notification)
                                    {
                                        warn!(?error, "could not forward local MCP notification");
                                    }
                                }
                                Ok(None) => {
                                    debug!(
                                        "not tunneling hop-scoped MCP cancellation through mcp/message"
                                    );
                                }
                                Err(error) => {
                                    warn!(?error, "could not forward local MCP notification");
                                }
                            }
                        }
                        Dispatch::Response(result, router) => {
                            if let Err(error) = router.route_with_result(result) {
                                debug!(?error, "could not route MCP client response");
                            }
                        }
                    }
                }

                BridgeMessage::ServerToClientRequest { request, responder } => {
                    match self.downstream_mode {
                        DownstreamMcpMode::Native => {
                            let pending = connection.send_request_to(Agent, request.raw);
                            if let Err(error) = pending.forward_response_to(responder) {
                                debug!(?error, "could not forward native MCP request");
                            }
                        }
                        DownstreamMcpMode::HttpAdapter => {
                            let connection_id = request.connection_id;
                            let Some(active) = self.bridge_connections.get_mut(&connection_id)
                            else {
                                respond_unknown_connection(responder, &connection_id);
                                continue;
                            };
                            let message = UntypedMessage {
                                method: request.method,
                                params: native_params_into_value(request.params),
                            };
                            if let Some(message) = active
                                .bridge
                                .try_send(Dispatch::Request(message, responder))
                            {
                                let Dispatch::Request(_, responder) = *message else {
                                    unreachable!("the failed bridge message was a request")
                                };
                                if let Err(send_error) = responder.respond_with_internal_error(
                                    "the local MCP client is unavailable or backpressured",
                                ) {
                                    debug!(
                                        ?send_error,
                                        "could not reject unavailable MCP connection"
                                    );
                                }
                            }
                        }
                        DownstreamMcpMode::Unknown | DownstreamMcpMode::Unavailable => {
                            if let Err(error) =
                                responder.respond_with_error(
                                    agent_client_protocol::Error::method_not_found(),
                                )
                            {
                                debug!(?error, "could not reject unsupported native MCP request");
                            }
                        }
                    }
                }

                BridgeMessage::ServerToClientNotification { notification } => {
                    match self.downstream_mode {
                        DownstreamMcpMode::Native => {
                            if let Err(error) =
                                connection.send_notification_to(Agent, notification.raw)
                            {
                                debug!(?error, "could not forward native MCP notification");
                            }
                        }
                        DownstreamMcpMode::HttpAdapter => {
                            let connection_id = notification.connection_id;
                            let Some(active) = self.bridge_connections.get_mut(&connection_id)
                            else {
                                debug!(
                                    connection_id,
                                    "ignoring notification for unknown MCP connection"
                                );
                                continue;
                            };
                            let message = UntypedMessage {
                                method: notification.method,
                                params: native_params_into_value(notification.params),
                            };
                            if active
                                .bridge
                                .try_send(Dispatch::Notification(message))
                                .is_some()
                            {
                                debug!("discarding MCP notification for unavailable local client");
                            }
                        }
                        DownstreamMcpMode::Unknown | DownstreamMcpMode::Unavailable => {
                            debug!("ignoring unsupported native MCP notification");
                        }
                    }
                }

                BridgeMessage::Disconnected { connection_id } => {
                    let Some(active) = self.bridge_connections.remove(&connection_id) else {
                        debug!(connection_id, "local MCP connection was already removed");
                        continue;
                    };
                    self.listeners.remove(&active.server_id);

                    let Some(protocol) = self.protocol else {
                        debug!("could not disconnect MCP bridge before ACP initialization");
                        continue;
                    };
                    let request = protocol.disconnect_request(connection_id)?;
                    let scheduled = connection
                        .send_request_to(Client, request)
                        .on_receiving_result(async move |result| {
                            match result {
                                Ok(response) => {
                                    if let Err(error) =
                                        protocol.validate_disconnect_response(response)
                                    {
                                        warn!(?error, "invalid response to mcp/disconnect");
                                    }
                                }
                                Err(error) => {
                                    debug!(?error, "mcp/disconnect failed");
                                }
                            }
                            Ok(())
                        });
                    if let Err(error) = scheduled {
                        debug!(
                            ?error,
                            "could not schedule mcp/disconnect response handling"
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

fn local_mcp_notification(
    protocol: PolyfillProtocol,
    connection_id: String,
    message: UntypedMessage,
) -> Result<Option<UntypedMessage>, agent_client_protocol::Error> {
    if is_cancel_request_notification(&message) {
        return Ok(None);
    }
    protocol
        .message_notification(connection_id, message)
        .map(Some)
}

fn reject_native_servers(
    protocol: PolyfillProtocol,
    servers: Vec<Value>,
    reason: &'static str,
) -> Result<Vec<Value>, agent_client_protocol::Error> {
    if servers
        .iter()
        .any(|server| protocol.native_server(server.clone()).is_some())
    {
        Err(agent_client_protocol::Error::invalid_params().data(reason))
    } else {
        Ok(servers)
    }
}

fn respond_unknown_connection(responder: Responder, connection_id: &str) {
    let error = agent_client_protocol::Error::invalid_params().data(serde_json::json!({
        "reason": "unknown MCP connection",
        "connectionId": connection_id,
    }));
    if let Err(send_error) = responder.respond_with_error(error) {
        debug!(
            ?send_error,
            connection_id, "could not reject unknown MCP connection"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use agent_client_protocol::{
        Conductor, Dispatch, ErrorCode, Proxy, UntypedMessage,
        schema::v1::{
            McpServer, McpServerAcp, McpServerHttp, MessageMcpNotification, MessageMcpRequest,
        },
    };
    use futures::{StreamExt, channel::mpsc};

    use super::{
        ActiveBridgeConnection, BridgeConnection, BridgeListener, BridgeListeners, BridgeRunner,
        DownstreamMcpMode, PolyfillHandler, PolyfillProtocol, local_mcp_notification,
        reject_native_servers,
    };

    #[test]
    fn http_declarations_reuse_endpoint_but_preserve_name_and_meta() {
        let listener = BridgeListener { tcp_port: 4321 };
        let first_meta = serde_json::Map::from_iter([("source".into(), "first".into())]);
        let second_meta = serde_json::Map::from_iter([("source".into(), "second".into())]);

        let first = PolyfillProtocol::V1
            .native_server(
                serde_json::to_value(McpServer::Acp(
                    McpServerAcp::new("first", "shared").meta(first_meta.clone()),
                ))
                .unwrap(),
            )
            .unwrap();
        let second = PolyfillProtocol::V1
            .native_server(
                serde_json::to_value(McpServer::Acp(
                    McpServerAcp::new("second", "shared").meta(second_meta.clone()),
                ))
                .unwrap(),
            )
            .unwrap();
        let first: McpServer =
            serde_json::from_value(listener.declaration(PolyfillProtocol::V1, first).unwrap())
                .unwrap();
        let second: McpServer =
            serde_json::from_value(listener.declaration(PolyfillProtocol::V1, second).unwrap())
                .unwrap();

        let McpServer::Http(first) = first else {
            panic!("expected HTTP declaration")
        };
        let McpServer::Http(second) = second else {
            panic!("expected HTTP declaration")
        };
        assert_eq!(first.url, "http://127.0.0.1:4321");
        assert_eq!(second.url, first.url);
        assert_eq!(first.name, "first");
        assert_eq!(second.name, "second");
        assert_eq!(first.meta, Some(first_meta));
        assert_eq!(second.meta, Some(second_meta));
    }

    #[test]
    fn downstream_mode_prefers_native_then_http_adaptation() {
        assert_eq!(
            DownstreamMcpMode::from_capabilities(true, true),
            DownstreamMcpMode::Native
        );
        assert_eq!(
            DownstreamMcpMode::from_capabilities(false, true),
            DownstreamMcpMode::Native
        );
        assert_eq!(
            DownstreamMcpMode::from_capabilities(true, false),
            DownstreamMcpMode::HttpAdapter
        );
        assert_eq!(
            DownstreamMcpMode::from_capabilities(false, false),
            DownstreamMcpMode::Unavailable
        );
    }

    #[test]
    fn local_cancellation_is_not_tunneled_as_an_mcp_message() {
        let cancellation = UntypedMessage {
            method: "$/cancel_request".to_string(),
            params: serde_json::json!({
                "requestId": "loopback-request"
            }),
        };
        assert_eq!(
            local_mcp_notification(
                PolyfillProtocol::V1,
                "native-connection".to_string(),
                cancellation,
            )
            .expect("cancellation filtering should not fail"),
            None
        );

        let notification = UntypedMessage {
            method: "notifications/progress".to_string(),
            params: serde_json::json!({
                "progressToken": "token",
                "progress": 0.5
            }),
        };
        let wrapped = local_mcp_notification(
            PolyfillProtocol::V1,
            "native-connection".to_string(),
            notification,
        )
        .expect("the notification should serialize")
        .expect("ordinary MCP notifications should be forwarded");
        assert_eq!(wrapped.method, "mcp/message");
        assert_eq!(
            wrapped.params["connectionId"],
            serde_json::json!("native-connection")
        );
        assert_eq!(
            wrapped.params["method"],
            serde_json::json!("notifications/progress")
        );
    }

    #[test]
    fn unavailable_mode_rejects_only_native_declarations() {
        let standard = vec![
            serde_json::to_value(McpServer::Http(McpServerHttp::new(
                "remote",
                "https://example.com/mcp",
            )))
            .unwrap(),
        ];
        assert_eq!(
            reject_native_servers(PolyfillProtocol::V1, standard.clone(), "unsupported").unwrap(),
            standard
        );

        let error = reject_native_servers(
            PolyfillProtocol::V1,
            vec![
                serde_json::to_value(McpServer::Acp(McpServerAcp::new("native", "server-1")))
                    .unwrap(),
            ],
            "unsupported",
        )
        .expect_err("native declarations require a downstream transport");
        assert_eq!(error.code, ErrorCode::InvalidParams);
        assert_eq!(error.data, Some(serde_json::json!("unsupported")));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reverse_messages_route_without_stopping_on_unknown_connections()
    -> Result<(), agent_client_protocol::Error> {
        let known_connection_id = "known-connection";
        let (bridge_tx, bridge_rx) = mpsc::channel(16);
        let (to_mcp_client_tx, mut to_mcp_client_rx) = mpsc::channel(16);
        let bridge_connections = HashMap::from([(
            known_connection_id.to_string(),
            ActiveBridgeConnection {
                server_id: "test-server".to_string(),
                bridge: BridgeConnection::new(to_mcp_client_tx),
            },
        )]);

        let proxy = Proxy
            .builder()
            .with_runner(BridgeRunner {
                bridge_tx: bridge_tx.clone(),
                bridge_rx,
                protocol: Some(PolyfillProtocol::V1),
                downstream_mode: DownstreamMcpMode::HttpAdapter,
                listeners: BridgeListeners::default(),
                bridge_connections,
            })
            .with_handler(PolyfillHandler {
                protocol: Some(PolyfillProtocol::V1),
                bridge_tx,
            });

        Conductor
            .builder()
            .connect_with(proxy, async move |connection| {
                let request_params = serde_json::Map::from_iter([(
                    "cursor".to_string(),
                    serde_json::json!("next-page"),
                )]);
                let request = MessageMcpRequest::new(known_connection_id, "tools/list")
                    .params(request_params.clone());
                let pending_response = connection.send_request(request);

                let Some(Dispatch::Request(message, responder)) = to_mcp_client_rx.next().await
                else {
                    panic!("expected the request to reach the stored bridge connection")
                };
                assert_eq!(message.method, "tools/list");
                assert_eq!(message.params, serde_json::Value::Object(request_params));

                let inner_response = serde_json::json!({"tools": [{"name": "echo"}]});
                responder.respond(inner_response.clone())?;
                let response = pending_response.block_task().await?;
                let response: serde_json::Value = serde_json::from_str(response.0.get())?;
                assert_eq!(response, inner_response);

                let unknown_error = connection
                    .send_request(MessageMcpRequest::new(
                        "missing-connection",
                        "resources/list",
                    ))
                    .block_task()
                    .await
                    .expect_err("an unknown connection must receive an error response");
                assert_eq!(unknown_error.code, ErrorCode::InvalidParams);
                assert_eq!(
                    unknown_error.data,
                    Some(serde_json::json!({
                        "reason": "unknown MCP connection",
                        "connectionId": "missing-connection",
                    }))
                );

                connection.send_notification(MessageMcpNotification::new(
                    "missing-connection",
                    "notifications/progress",
                ))?;
                connection.send_notification(MessageMcpNotification::new(
                    known_connection_id,
                    "notifications/tools/list_changed",
                ))?;

                let Some(Dispatch::Notification(notification)) = to_mcp_client_rx.next().await
                else {
                    panic!("expected the known notification after ignoring the unknown one")
                };
                assert_eq!(notification.method, "notifications/tools/list_changed");
                assert_eq!(notification.params, serde_json::Value::Null);

                Ok(())
            })
            .await
    }
}
