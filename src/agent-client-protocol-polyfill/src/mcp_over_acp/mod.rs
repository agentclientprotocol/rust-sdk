//! MCP-over-ACP polyfill proxy.
//!
//! This proxy bridges MCP-over-ACP transport for agents that don't support
//! `mcpCapabilities.acp` natively. It sits in the proxy chain and:
//!
//! - Intercepts `NewSessionRequest` to transform `McpServer::Http` entries with `acp:` URLs
//!   into localhost TCP bridges
//! - Handles `_mcp/connect`, `_mcp/message`, `_mcp/disconnect` by routing through those bridges
//!
//! # Usage
//!
//! ```rust,ignore
//! use agent_client_protocol_polyfill::mcp_over_acp::McpOverAcpPolyfill;
//!
//! // Add to a conductor proxy chain
//! let conductor = ConductorImpl::new_agent(
//!     "conductor",
//!     ProxiesAndAgent::new(my_agent).proxy(McpOverAcpPolyfill::http()),
//!     McpBridgeMode::default(),
//! );
//! ```

mod actor;
pub(crate) mod http;
pub(crate) mod stdio;

use std::collections::HashMap;
use std::path::PathBuf;

use agent_client_protocol::schema::v1::{
    McpServer, McpServerHttp, McpServerStdio, NewSessionRequest,
};
use agent_client_protocol::schema::{
    InitializeProxyRequest, McpConnectRequest, McpConnectResponse, McpDisconnectNotification,
    McpOverAcpMessage,
};
use agent_client_protocol::{
    Agent, Client, Conductor, ConnectTo, ConnectionTo, Dispatch, Proxy, Role,
};
use futures::{SinkExt, channel::mpsc};
use tokio::net::TcpListener;
use tracing::info;

use self::actor::BridgeConnectionActor;

/// Internal messages for the polyfill's bridge management.
#[derive(Debug)]
pub(crate) enum BridgeMessage {
    /// A new TCP connection was accepted and needs an ACP connection ID.
    ConnectionReceived {
        acp_id: String,
        actor: BridgeConnectionActor,
        connection: BridgeConnection,
    },

    /// ACP connection ID received — spawn the actor and store the connection.
    ConnectionEstablished {
        response: McpConnectResponse,
        actor: BridgeConnectionActor,
        connection: BridgeConnection,
    },

    /// MCP message from a bridge client that needs to be forwarded over ACP.
    ClientToServer {
        connection_id: String,
        message: Dispatch,
    },

    /// Bridge client disconnected.
    Disconnected {
        notification: McpDisconnectNotification,
    },
}

/// Connection handle for sending messages to an MCP client via a bridge.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct BridgeConnection {
    to_mcp_client_tx: mpsc::Sender<Dispatch>,
}

impl BridgeConnection {
    pub fn new(to_mcp_client_tx: mpsc::Sender<Dispatch>) -> Self {
        Self { to_mcp_client_tx }
    }

    #[allow(dead_code)]
    pub async fn send(&mut self, message: Dispatch) -> Result<(), agent_client_protocol::Error> {
        self.to_mcp_client_tx
            .send(message)
            .await
            .map_err(|_| agent_client_protocol::Error::internal_error())
    }
}

/// Mode for the MCP bridge transport.
#[derive(Debug, Clone, Default)]
pub enum BridgeMode {
    /// Use stdio-based MCP bridge with a subprocess.
    Stdio {
        /// Command and args to spawn bridge processes.
        conductor_command: Vec<String>,
    },

    /// Use HTTP-based MCP bridge (default).
    #[default]
    Http,
}

/// MCP-over-ACP polyfill proxy.
///
/// Bridges MCP-over-ACP transport for agents that don't support `mcpCapabilities.acp`.
#[derive(Debug)]
pub struct McpOverAcpPolyfill {
    mode: BridgeMode,
}

impl McpOverAcpPolyfill {
    /// Create a polyfill using HTTP bridge mode.
    #[must_use]
    pub fn http() -> Self {
        Self {
            mode: BridgeMode::Http,
        }
    }

    /// Create a polyfill using stdio bridge mode.
    #[must_use]
    pub fn stdio(conductor_command: Vec<String>) -> Self {
        Self {
            mode: BridgeMode::Stdio { conductor_command },
        }
    }
}

impl ConnectTo<Conductor> for McpOverAcpPolyfill {
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol::Error> {
        let (bridge_tx, bridge_rx) = mpsc::channel(128);
        let mode = self.mode;

        Proxy
            .builder()
            .name("mcp-over-acp-polyfill")
            .with_responder(BridgeResponder {
                bridge_tx: bridge_tx.clone(),
                bridge_rx,
                bridge_connections: HashMap::new(),
            })
            .on_receive_request_from(
                Client,
                async move |request: InitializeProxyRequest,
                            responder,
                            cx: ConnectionTo<Conductor>| {
                    // Forward initialize to successor, then set mcpCapabilities.acp = true
                    // in the response to advertise that we handle MCP-over-ACP.
                    cx.send_request_to(Agent, request.initialize)
                        .on_receiving_result(async move |result| {
                            responder.respond_with_result(result.map(|mut response| {
                                response.agent_capabilities.mcp_capabilities.acp = true;
                                response
                            }))
                        })
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request_from(
                Client,
                {
                    let bridge_tx = bridge_tx.clone();
                    async move |mut request: NewSessionRequest,
                                responder,
                                cx: ConnectionTo<Conductor>| {
                        // Transform acp: URLs in MCP servers
                        let mut listeners = BridgeListeners::default();
                        for mcp_server in &mut request.mcp_servers {
                            listeners
                                .transform_mcp_server(cx.clone(), mcp_server, &bridge_tx, &mode)
                                .await?;
                        }
                        // Forward modified request to successor
                        cx.send_request_to(Agent, request)
                            .forward_response_to(responder)
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}

/// Manages active bridge listeners (TCP listeners for acp: URLs).
#[derive(Default, Debug)]
struct BridgeListeners {
    listeners: HashMap<String, BridgeListener>,
}

#[derive(Clone, Debug)]
struct BridgeListener {
    server: McpServer,
}

impl BridgeListeners {
    /// Transform an MCP server with `acp:` URL into a bridged localhost server.
    async fn transform_mcp_server(
        &mut self,
        connection: ConnectionTo<impl Role>,
        mcp_server: &mut McpServer,
        bridge_tx: &mpsc::Sender<BridgeMessage>,
        mode: &BridgeMode,
    ) -> Result<(), agent_client_protocol::Error> {
        let McpServer::Http(http) = mcp_server else {
            return Ok(());
        };

        if !http.url.starts_with("acp:") {
            return Ok(());
        }

        if !http.headers.is_empty() {
            return Err(agent_client_protocol::Error::internal_error());
        }

        let name = http.name.clone();
        let url = http.url.clone();

        info!(
            server_name = %name,
            acp_id = %url,
            "Detected MCP server with ACP transport, spawning TCP bridge"
        );

        let transformed = self
            .spawn_bridge(connection, &name, &url, bridge_tx, mode)
            .await?;
        *mcp_server = transformed;
        Ok(())
    }

    async fn spawn_bridge(
        &mut self,
        connection: ConnectionTo<impl Role>,
        server_name: &str,
        acp_id: &str,
        bridge_tx: &mpsc::Sender<BridgeMessage>,
        mode: &BridgeMode,
    ) -> anyhow::Result<McpServer> {
        if let Some(listener) = self.listeners.get(acp_id) {
            return Ok(listener.server.clone());
        }

        let tcp_listener = TcpListener::bind("127.0.0.1:0").await?;
        let tcp_port = tcp_listener.local_addr()?.port();

        info!(acp_id = acp_id, tcp_port, "Bound listener for MCP bridge");

        let new_server = match mode {
            BridgeMode::Stdio { conductor_command } => McpServer::Stdio(
                McpServerStdio::new(
                    server_name.to_string(),
                    PathBuf::from(&conductor_command[0]),
                )
                .args(
                    conductor_command[1..]
                        .iter()
                        .cloned()
                        .chain(vec!["mcp".to_string(), format!("{tcp_port}")])
                        .collect::<Vec<_>>(),
                ),
            ),

            BridgeMode::Http => McpServer::Http(McpServerHttp::new(
                server_name.to_string(),
                format!("http://localhost:{tcp_port}"),
            )),
        };

        self.listeners.insert(
            acp_id.to_string(),
            BridgeListener {
                server: new_server.clone(),
            },
        );

        connection.spawn({
            let acp_id = acp_id.to_string();
            let bridge_tx = bridge_tx.clone();
            let mode = mode.clone();
            async move {
                info!(
                    acp_id = acp_id,
                    tcp_port, "now accepting bridge connections"
                );
                match mode {
                    BridgeMode::Stdio {
                        conductor_command: _,
                    } => stdio::run_tcp_listener(tcp_listener, acp_id, bridge_tx).await,
                    BridgeMode::Http => {
                        http::run_http_listener(tcp_listener, acp_id, bridge_tx).await
                    }
                }
            }
        })?;

        Ok(new_server)
    }
}

/// Responder that runs alongside the proxy, managing bridge state.
struct BridgeResponder {
    bridge_tx: mpsc::Sender<BridgeMessage>,
    bridge_rx: mpsc::Receiver<BridgeMessage>,
    bridge_connections: HashMap<String, BridgeConnection>,
}

impl std::fmt::Debug for BridgeResponder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeResponder")
            .field("bridge_connections", &self.bridge_connections.len())
            .finish_non_exhaustive()
    }
}

impl agent_client_protocol::RunWithConnectionTo<Conductor> for BridgeResponder {
    async fn run_with_connection_to(
        mut self,
        connection: ConnectionTo<Conductor>,
    ) -> Result<(), agent_client_protocol::Error> {
        use futures::StreamExt;

        while let Some(message) = self.bridge_rx.next().await {
            match message {
                BridgeMessage::ConnectionReceived {
                    acp_id,
                    actor,
                    connection: bridge_conn,
                } => {
                    // Send _mcp/connect request back through the chain.
                    // When the response arrives, send ConnectionEstablished back to ourselves.
                    connection
                        .send_request_to(Client, McpConnectRequest { acp_id, meta: None })
                        .on_receiving_result({
                            let mut bridge_tx = self.bridge_tx.clone();
                            async move |result| match result {
                                Ok(response) => bridge_tx
                                    .send(BridgeMessage::ConnectionEstablished {
                                        response,
                                        actor,
                                        connection: bridge_conn,
                                    })
                                    .await
                                    .map_err(|_| agent_client_protocol::Error::internal_error()),
                                Err(_) => Ok(()),
                            }
                        })?;
                }

                BridgeMessage::ConnectionEstablished {
                    response: McpConnectResponse { connection_id, .. },
                    actor,
                    connection: bridge_conn,
                } => {
                    self.bridge_connections
                        .insert(connection_id.clone(), bridge_conn);
                    connection.spawn(actor.run(connection_id))?;
                }

                BridgeMessage::ClientToServer {
                    connection_id,
                    message,
                } => {
                    let wrapped = message.map(
                        |request, responder| {
                            (
                                McpOverAcpMessage {
                                    connection_id: connection_id.clone(),
                                    message: request,
                                    meta: None,
                                },
                                responder,
                            )
                        },
                        |notification| McpOverAcpMessage {
                            connection_id: connection_id.clone(),
                            message: notification,
                            meta: None,
                        },
                    );
                    connection.send_proxied_message_to(Client, wrapped)?;
                }

                BridgeMessage::Disconnected { notification } => {
                    self.bridge_connections.remove(&notification.connection_id);
                    connection.send_notification_to(Client, notification)?;
                }
            }
        }
        Ok(())
    }
}
