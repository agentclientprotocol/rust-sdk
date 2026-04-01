pub mod actor;
pub mod http;
pub mod stdio;

use std::collections::HashMap;
use std::path::PathBuf;

use futures::{SinkExt, channel::mpsc};
use agent_client_protocol_core::schema::{McpServer, McpServerHttp, McpServerStdio};
use agent_client_protocol_core::{ConnectionTo, Dispatch, Role};
use tokio::net::TcpListener;
use tracing::info;

pub use self::actor::McpBridgeConnectionActor;
use crate::conductor::ConductorMessage;

/// Maintains bridges for MCP message routing.
#[derive(Default)]
pub struct McpBridgeListeners {
    /// Mapping of acp:$UUID URLs to TCP bridge information for MCP message routing
    listeners: HashMap<String, McpBridgeListener>,
}

/// Information about an MCP bridge that is listening for connections from MCP clients.
#[derive(Clone, Debug)]
pub(super) struct McpBridgeListener {
    /// The replacement MCP server
    pub server: McpServer,
}

/// Connection handle for sending messages to an MCP client.
#[derive(Clone, Debug)]
pub struct McpBridgeConnection {
    /// Channel to send messages from MCP server (ACP proxy) to the MCP client (ACP agent).
    to_mcp_client_tx: mpsc::Sender<Dispatch>,
}

impl McpBridgeConnection {
    pub fn new(to_mcp_client_tx: mpsc::Sender<Dispatch>) -> Self {
        Self { to_mcp_client_tx }
    }

    pub async fn send(&mut self, message: Dispatch) -> Result<(), agent_client_protocol_core::Error> {
        self.to_mcp_client_tx
            .send(message)
            .await
            .map_err(|_| agent_client_protocol_core::Error::internal_error())
    }
}

impl McpBridgeListeners {
    /// Transforms MCP servers with `acp:$UUID` URLs for agents that need bridging.
    ///
    /// For each MCP server with an `acp:` URL:
    /// 1. Spawns a TCP listener on an ephemeral port
    /// 2. Stores the mapping for message routing
    /// 3. Transforms the server to use either stdio or HTTP transport depending on bridge mode
    ///
    /// Other MCP servers are left unchanged.
    pub async fn transform_mcp_server(
        &mut self,
        connection: ConnectionTo<impl Role>,
        mcp_server: &mut McpServer,
        conductor_tx: &mpsc::Sender<ConductorMessage>,
        mcp_bridge_mode: &crate::McpBridgeMode,
    ) -> Result<(), agent_client_protocol_core::Error> {
        use agent_client_protocol_core::schema::McpServer;

        let McpServer::Http(http) = mcp_server else {
            return Ok(());
        };

        if !http.url.starts_with("acp:") {
            return Ok(());
        }

        if !http.headers.is_empty() {
            return Err(agent_client_protocol_core::Error::internal_error());
        }

        let name = &http.name;
        let url = &http.url;

        info!(
            server_name = name,
            acp_url = url,
            "Detected MCP server with ACP transport, spawning TCP bridge"
        );

        // Create oneshot channel for session_id delivery
        let transformed = self
            .spawn_bridge(connection, name, url, conductor_tx, mcp_bridge_mode)
            .await?;
        *mcp_server = transformed;
        Ok(())
    }

    /// Spawn a bridge listener (HTTP or stdio) for an MCP server with ACP transport
    async fn spawn_bridge(
        &mut self,
        connection: ConnectionTo<impl Role>,
        server_name: &str,
        acp_url: &str,
        conductor_tx: &mpsc::Sender<ConductorMessage>,
        mcp_bridge_mode: &crate::McpBridgeMode,
    ) -> anyhow::Result<McpServer> {
        // If there is already a listener for the ACP URL, return its server
        if let Some(listener) = self.listeners.get(acp_url) {
            return Ok(listener.server.clone());
        }

        // Bind to ephemeral port
        let tcp_listener = TcpListener::bind("127.0.0.1:0").await?;
        let tcp_port = tcp_listener.local_addr()?.port();

        info!(acp_url = acp_url, tcp_port, "Bound listener for MCP bridge");

        let new_server = match mcp_bridge_mode {
            crate::McpBridgeMode::Stdio { conductor_command } => McpServer::Stdio(
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

            crate::McpBridgeMode::Http => McpServer::Http(McpServerHttp::new(
                server_name.to_string(),
                format!("http://localhost:{tcp_port}"),
            )),
        };

        // remember for later
        self.listeners.insert(
            acp_url.to_string(),
            McpBridgeListener {
                server: new_server.clone(),
            },
        );

        connection.spawn({
            let acp_url = acp_url.to_string();
            let conductor_tx = conductor_tx.clone();
            let mcp_bridge_mode = mcp_bridge_mode.clone();
            async move {
                info!(
                    acp_url = acp_url,
                    tcp_port, "now accepting bridge connections"
                );

                match mcp_bridge_mode {
                    crate::McpBridgeMode::Stdio {
                        conductor_command: _,
                    } => stdio::run_tcp_listener(tcp_listener, acp_url, conductor_tx).await,
                    crate::McpBridgeMode::Http => {
                        http::run_http_listener(tcp_listener, acp_url, conductor_tx).await
                    }
                }
            }
        })?;

        Ok(new_server)
    }
}
