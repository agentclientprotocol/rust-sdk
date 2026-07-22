//! MCP-over-ACP compatibility proxy.
//!
//! This proxy adapts schema-native [`McpServer::Acp`] declarations for agents that do not
//! support the ACP MCP transport. It replaces those declarations with loopback HTTP bridges and
//! relays `mcp/connect`, `mcp/message`, and `mcp/disconnect` over ACP.
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

use std::collections::HashMap;

use agent_client_protocol::{
    Agent, Client, Conductor, ConnectTo, ConnectionTo, Dispatch, Handled, Proxy, Responder,
    UntypedMessage,
    schema::{
        InitializeProxyRequest,
        v1::{
            AgentNotification, AgentRequest, ConnectMcpRequest, ConnectMcpResponse,
            DisconnectMcpRequest, DisconnectMcpResponse, LoadSessionRequest, McpConnectionId,
            McpServer, McpServerAcp, McpServerHttp, MessageMcpNotification, MessageMcpRequest,
            NewSessionRequest, ResumeSessionRequest,
        },
    },
};
use futures::{SinkExt, channel::mpsc, channel::oneshot};
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

use self::actor::BridgeConnectionActor;

#[cfg(feature = "unstable_session_fork")]
use agent_client_protocol::schema::v1::ForkSessionRequest;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DownstreamMcpMode {
    #[default]
    Unknown,
    Native,
    HttpAdapter,
    Unavailable,
}

impl DownstreamMcpMode {
    fn from_capabilities(http: bool, acp: bool) -> Self {
        if acp {
            Self::Native
        } else if http {
            Self::HttpAdapter
        } else {
            Self::Unavailable
        }
    }
}

/// Internal messages for the polyfill's bridge management.
#[derive(Debug)]
pub(crate) enum BridgeMessage {
    /// Record which MCP transport the successor can consume.
    SetDownstreamMode(DownstreamMcpMode),

    /// Transform the MCP declarations for one session setup request.
    TransformServers {
        servers: Vec<McpServer>,
        response_tx: oneshot::Sender<Result<Vec<McpServer>, agent_client_protocol::Error>>,
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
        connection_id: McpConnectionId,
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
        request: MessageMcpRequest,
        responder: Responder,
    },

    /// An MCP server notification received over ACP for the local agent's MCP client.
    ServerToClientNotification {
        notification: MessageMcpNotification,
    },

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

        let builder = Proxy
            .builder()
            .name("mcp-over-acp-polyfill")
            .with_runner(BridgeRunner {
                bridge_tx: bridge_tx.clone(),
                bridge_rx,
                downstream_mode: DownstreamMcpMode::Unknown,
                listeners: BridgeListeners::default(),
                bridge_connections: HashMap::new(),
            })
            .on_receive_request_from(
                Client,
                {
                    let bridge_tx = bridge_tx.clone();
                    async move |request: InitializeProxyRequest,
                                responder,
                                cx: ConnectionTo<Conductor>| {
                        let mut response_bridge_tx = bridge_tx.clone();
                        cx.send_request_to(Agent, request.initialize)
                            .on_receiving_result(async move |result| {
                                let result = match result {
                                    Ok(mut response) => {
                                        let capabilities =
                                            &mut response.agent_capabilities.mcp_capabilities;
                                        let mode = DownstreamMcpMode::from_capabilities(
                                            capabilities.http,
                                            capabilities.acp,
                                        );
                                        response_bridge_tx
                                            .send(BridgeMessage::SetDownstreamMode(mode))
                                            .await
                                            .map_err(
                                                agent_client_protocol::Error::into_internal_error,
                                            )?;
                                        if mode == DownstreamMcpMode::HttpAdapter {
                                            capabilities.acp = true;
                                        }
                                        Ok(response)
                                    }
                                    Err(error) => Err(error),
                                };
                                responder.respond_with_result(result)
                            })
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request_from(
                Client,
                {
                    let mut bridge_tx = bridge_tx.clone();
                    async move |mut request: NewSessionRequest,
                                responder,
                                cx: ConnectionTo<Conductor>| {
                        transform_session_servers(&mut request.mcp_servers, &mut bridge_tx).await?;
                        cx.send_request_to(Agent, request)
                            .forward_response_to(responder)
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request_from(
                Client,
                {
                    let mut bridge_tx = bridge_tx.clone();
                    async move |mut request: LoadSessionRequest,
                                responder,
                                cx: ConnectionTo<Conductor>| {
                        transform_session_servers(&mut request.mcp_servers, &mut bridge_tx).await?;
                        cx.send_request_to(Agent, request)
                            .forward_response_to(responder)
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request_from(
                Client,
                {
                    let mut bridge_tx = bridge_tx.clone();
                    async move |mut request: ResumeSessionRequest,
                                responder,
                                cx: ConnectionTo<Conductor>| {
                        transform_session_servers(&mut request.mcp_servers, &mut bridge_tx).await?;
                        cx.send_request_to(Agent, request)
                            .forward_response_to(responder)
                    }
                },
                agent_client_protocol::on_receive_request!(),
            );

        #[cfg(feature = "unstable_session_fork")]
        let builder = builder.on_receive_request_from(
            Client,
            {
                let mut bridge_tx = bridge_tx.clone();
                async move |mut request: ForkSessionRequest,
                            responder,
                            cx: ConnectionTo<Conductor>| {
                    transform_session_servers(&mut request.mcp_servers, &mut bridge_tx).await?;
                    cx.send_request_to(Agent, request)
                        .forward_response_to(responder)
                }
            },
            agent_client_protocol::on_receive_request!(),
        );

        builder
            .on_receive_request_from(
                Client,
                {
                    let mut bridge_tx = bridge_tx.clone();
                    async move |request: MessageMcpRequest, responder, _cx| {
                        bridge_tx
                            .send(BridgeMessage::ServerToClientRequest {
                                request,
                                responder: responder.erase_to_json(),
                            })
                            .await
                            .map_err(agent_client_protocol::Error::into_internal_error)?;
                        Ok(Handled::Yes)
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification_from(
                Client,
                {
                    let mut bridge_tx = bridge_tx.clone();
                    async move |notification: MessageMcpNotification, _cx| {
                        bridge_tx
                            .send(BridgeMessage::ServerToClientNotification { notification })
                            .await
                            .map_err(agent_client_protocol::Error::into_internal_error)
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_to(client)
            .await
    }
}

async fn transform_session_servers(
    servers: &mut Vec<McpServer>,
    bridge_tx: &mut mpsc::Sender<BridgeMessage>,
) -> Result<(), agent_client_protocol::Error> {
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
    fn declaration(&self, server: McpServerAcp) -> McpServer {
        McpServer::Http(
            McpServerHttp::new(server.name, format!("http://127.0.0.1:{}", self.tcp_port))
                .meta(server.meta),
        )
    }
}

impl BridgeListeners {
    async fn transform_servers(
        &mut self,
        connection: &ConnectionTo<Conductor>,
        servers: Vec<McpServer>,
        bridge_tx: &mpsc::Sender<BridgeMessage>,
    ) -> Result<Vec<McpServer>, agent_client_protocol::Error> {
        let mut transformed = Vec::with_capacity(servers.len());
        for server in servers {
            transformed.push(self.transform_server(connection, server, bridge_tx).await?);
        }
        Ok(transformed)
    }

    async fn transform_server(
        &mut self,
        connection: &ConnectionTo<Conductor>,
        server: McpServer,
        bridge_tx: &mpsc::Sender<BridgeMessage>,
    ) -> Result<McpServer, agent_client_protocol::Error> {
        let McpServer::Acp(acp_server) = server else {
            return Ok(server);
        };
        let server_id = acp_server.server_id.to_string();

        info!(
            server_name = %acp_server.name,
            server_id,
            "detected native MCP-over-ACP server; creating compatibility bridge"
        );

        if let Some(listener) = self.listeners.get(&server_id) {
            return Ok(listener.declaration(acp_server));
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

        let declaration = listener.declaration(acp_server);
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
    downstream_mode: DownstreamMcpMode,
    listeners: BridgeListeners,
    bridge_connections: HashMap<String, ActiveBridgeConnection>,
}

impl std::fmt::Debug for BridgeRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeRunner")
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
                BridgeMessage::SetDownstreamMode(mode) => {
                    self.downstream_mode = mode;
                }

                BridgeMessage::TransformServers {
                    servers,
                    response_tx,
                } => {
                    let result = match self.downstream_mode {
                        DownstreamMcpMode::Native => Ok(servers),
                        DownstreamMcpMode::HttpAdapter => {
                            self.listeners
                                .transform_servers(&connection, servers, &self.bridge_tx)
                                .await
                        }
                        DownstreamMcpMode::Unavailable => reject_native_servers(
                            servers,
                            "the downstream agent supports neither native nor HTTP MCP transport",
                        ),
                        DownstreamMcpMode::Unknown => reject_native_servers(
                            servers,
                            "MCP transport capabilities are unavailable before initialize",
                        ),
                    };
                    drop(response_tx.send(result));
                }

                BridgeMessage::ConnectionReceived {
                    server_id,
                    actor,
                    connection: bridge,
                } => {
                    let request =
                        AgentRequest::ConnectMcpRequest(ConnectMcpRequest::new(server_id.clone()));
                    let mut bridge_tx = self.bridge_tx.clone();
                    let scheduled = connection
                        .send_request_to(Client, request)
                        .on_receiving_result(async move |result| {
                            let message = match result {
                                Ok(response) => {
                                    match serde_json::from_value::<ConnectMcpResponse>(response) {
                                        Ok(ConnectMcpResponse { connection_id, .. }) => {
                                            BridgeMessage::ConnectionEstablished {
                                                server_id,
                                                connection_id,
                                                actor,
                                                connection: bridge,
                                            }
                                        }
                                        Err(error) => {
                                            warn!(?error, "invalid response to mcp/connect");
                                            BridgeMessage::ConnectionFailed { server_id }
                                        }
                                    }
                                }
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
                    let connection_id = connection_id.to_string();
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
                } => match message {
                    Dispatch::Request(message, responder) => {
                        match message_mcp_request(connection_id, message) {
                            Ok(request) => {
                                let pending = connection.send_request_to(
                                    Client,
                                    AgentRequest::MessageMcpRequest(request),
                                );
                                if let Err(error) = pending.forward_response_to(responder) {
                                    warn!(?error, "could not forward local MCP request response");
                                }
                            }
                            Err(error) => {
                                if let Err(send_error) = responder.respond_with_error(error) {
                                    debug!(?send_error, "could not reject malformed MCP request");
                                }
                            }
                        }
                    }
                    Dispatch::Notification(message) => {
                        match message_mcp_notification(connection_id, message) {
                            Ok(notification) => {
                                if let Err(error) = connection.send_notification_to(
                                    Client,
                                    AgentNotification::MessageMcpNotification(notification),
                                ) {
                                    warn!(?error, "could not forward local MCP notification");
                                }
                            }
                            Err(error) => {
                                warn!(?error, "discarding malformed local MCP notification");
                            }
                        }
                    }
                    Dispatch::Response(result, router) => {
                        if let Err(error) = router.route_with_result(result) {
                            debug!(?error, "could not route MCP client response");
                        }
                    }
                },

                BridgeMessage::ServerToClientRequest { request, responder } => {
                    match self.downstream_mode {
                        DownstreamMcpMode::Native => {
                            let pending = connection
                                .send_request_to(Agent, AgentRequest::MessageMcpRequest(request));
                            if let Err(error) = pending.forward_response_to(responder) {
                                debug!(?error, "could not forward native MCP request");
                            }
                        }
                        DownstreamMcpMode::HttpAdapter => {
                            let connection_id = request.connection_id.to_string();
                            let Some(active) = self.bridge_connections.get_mut(&connection_id)
                            else {
                                respond_unknown_connection(responder, &connection_id);
                                continue;
                            };
                            let message = message_mcp_request_to_untyped(request);
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
                            if let Err(error) = connection.send_notification_to(
                                Agent,
                                AgentNotification::MessageMcpNotification(notification),
                            ) {
                                debug!(?error, "could not forward native MCP notification");
                            }
                        }
                        DownstreamMcpMode::HttpAdapter => {
                            let connection_id = notification.connection_id.to_string();
                            let Some(active) = self.bridge_connections.get_mut(&connection_id)
                            else {
                                debug!(
                                    connection_id,
                                    "ignoring notification for unknown MCP connection"
                                );
                                continue;
                            };
                            let message = message_mcp_notification_to_untyped(notification);
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

                    let request = AgentRequest::DisconnectMcpRequest(DisconnectMcpRequest::new(
                        connection_id,
                    ));
                    let scheduled = connection
                        .send_request_to(Client, request)
                        .on_receiving_result(async |result| {
                            match result {
                                Ok(response) => {
                                    if let Err(error) =
                                        serde_json::from_value::<DisconnectMcpResponse>(response)
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

fn reject_native_servers(
    servers: Vec<McpServer>,
    reason: &'static str,
) -> Result<Vec<McpServer>, agent_client_protocol::Error> {
    if servers
        .iter()
        .any(|server| matches!(server, McpServer::Acp(_)))
    {
        Err(agent_client_protocol::Error::invalid_params().data(reason))
    } else {
        Ok(servers)
    }
}

fn into_mcp_params(
    params: serde_json::Value,
) -> Result<Option<serde_json::Map<String, serde_json::Value>>, agent_client_protocol::Error> {
    match params {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Object(params) => Ok(Some(params)),
        params => Err(
            agent_client_protocol::Error::invalid_params().data(serde_json::json!({
                "reason": "MCP message params must be an object or null",
                "params": params,
            })),
        ),
    }
}

fn message_mcp_request(
    connection_id: String,
    message: UntypedMessage,
) -> Result<MessageMcpRequest, agent_client_protocol::Error> {
    let (method, params) = message.into_parts();
    let mut request = MessageMcpRequest::new(connection_id, method);
    request.params = into_mcp_params(params)?;
    Ok(request)
}

fn message_mcp_notification(
    connection_id: String,
    message: UntypedMessage,
) -> Result<MessageMcpNotification, agent_client_protocol::Error> {
    let (method, params) = message.into_parts();
    let mut notification = MessageMcpNotification::new(connection_id, method);
    notification.params = into_mcp_params(params)?;
    Ok(notification)
}

fn message_mcp_request_to_untyped(request: MessageMcpRequest) -> UntypedMessage {
    UntypedMessage {
        method: request.method,
        params: request
            .params
            .map_or(serde_json::Value::Null, serde_json::Value::Object),
    }
}

fn message_mcp_notification_to_untyped(notification: MessageMcpNotification) -> UntypedMessage {
    UntypedMessage {
        method: notification.method,
        params: notification
            .params
            .map_or(serde_json::Value::Null, serde_json::Value::Object),
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
        Client, Conductor, Dispatch, ErrorCode, Handled, Proxy,
        schema::v1::{
            McpServer, McpServerAcp, McpServerHttp, MessageMcpNotification, MessageMcpRequest,
        },
    };
    use futures::{SinkExt, StreamExt, channel::mpsc};

    use super::{
        ActiveBridgeConnection, BridgeConnection, BridgeListener, BridgeListeners, BridgeMessage,
        BridgeRunner, DownstreamMcpMode, reject_native_servers,
    };

    #[test]
    fn http_declarations_reuse_endpoint_but_preserve_name_and_meta() {
        let listener = BridgeListener { tcp_port: 4321 };
        let first_meta = serde_json::Map::from_iter([("source".into(), "first".into())]);
        let second_meta = serde_json::Map::from_iter([("source".into(), "second".into())]);

        let first =
            listener.declaration(McpServerAcp::new("first", "shared").meta(first_meta.clone()));
        let second =
            listener.declaration(McpServerAcp::new("second", "shared").meta(second_meta.clone()));

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
    fn unavailable_mode_rejects_only_native_declarations() {
        let standard = vec![McpServer::Http(McpServerHttp::new(
            "remote",
            "https://example.com/mcp",
        ))];
        assert_eq!(
            reject_native_servers(standard.clone(), "unsupported").unwrap(),
            standard
        );

        let error = reject_native_servers(
            vec![McpServer::Acp(McpServerAcp::new("native", "server-1"))],
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
                downstream_mode: DownstreamMcpMode::HttpAdapter,
                listeners: BridgeListeners::default(),
                bridge_connections,
            })
            .on_receive_request_from(
                Client,
                {
                    let mut bridge_tx = bridge_tx.clone();
                    async move |request: MessageMcpRequest, responder, _cx| {
                        bridge_tx
                            .send(BridgeMessage::ServerToClientRequest {
                                request,
                                responder: responder.erase_to_json(),
                            })
                            .await
                            .map_err(agent_client_protocol::Error::into_internal_error)?;
                        Ok(Handled::Yes)
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification_from(
                Client,
                {
                    let mut bridge_tx = bridge_tx;
                    async move |notification: MessageMcpNotification, _cx| {
                        bridge_tx
                            .send(BridgeMessage::ServerToClientNotification { notification })
                            .await
                            .map_err(agent_client_protocol::Error::into_internal_error)
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            );

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
