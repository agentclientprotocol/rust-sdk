use agent_client_protocol_core::{
    ConnectTo, Dispatch, DynConnectTo, role::mcp, schema::McpDisconnectNotification,
};
use futures::{SinkExt as _, StreamExt as _, channel::mpsc};
use tracing::info;

use crate::conductor::ConductorMessage;

/// Trait for actors that handle MCP bridge connections.
///
/// Implementations bridge between MCP clients and the conductor's ACP message flow.
#[derive(Debug)]
pub struct McpBridgeConnectionActor {
    /// How to connect to the MCP server
    transport: DynConnectTo<mcp::Client>,

    /// Sender for messages to the conductor
    conductor_tx: mpsc::Sender<ConductorMessage>,

    /// Receiver for messages from the conductor to the MCP client
    to_mcp_client_rx: mpsc::Receiver<Dispatch>,
}

impl McpBridgeConnectionActor {
    pub fn new(
        component: impl ConnectTo<mcp::Client>,
        conductor_tx: mpsc::Sender<ConductorMessage>,
        to_mcp_client_rx: mpsc::Receiver<Dispatch>,
    ) -> Self {
        Self {
            transport: DynConnectTo::new(component),
            conductor_tx,
            to_mcp_client_rx,
        }
    }

    pub async fn run(self, connection_id: String) -> Result<(), agent_client_protocol_core::Error> {
        info!(connection_id, "MCP bridge connected");

        let McpBridgeConnectionActor {
            transport,
            mut conductor_tx,
            to_mcp_client_rx,
        } = self;

        let client = mcp::Client
            .builder()
            .name(format!("mpc-client-to-conductor({connection_id})"))
            // When we receive a message from the MCP client, forward it to the conductor
            .on_receive_dispatch(
                {
                    let mut conductor_tx = conductor_tx.clone();
                    let connection_id = connection_id.clone();
                    async move |message: agent_client_protocol_core::Dispatch, _cx| {
                        conductor_tx
                            .send(ConductorMessage::McpClientToMcpServer {
                                connection_id: connection_id.clone(),
                                message,
                            })
                            .await
                            .map_err(|_| agent_client_protocol_core::Error::internal_error())
                    }
                },
                agent_client_protocol_core::on_receive_dispatch!(),
            )
            // When we receive messages from the conductor, forward them to the MCP client
            .connect_with(transport, async move |mcp_connection_to_client| {
                let mut to_mcp_client_rx = to_mcp_client_rx;
                while let Some(message) = to_mcp_client_rx.next().await {
                    mcp_connection_to_client.send_proxied_message(message)?;
                }
                Ok(())
            });
        let result = Box::pin(client).await;

        conductor_tx
            .send(ConductorMessage::McpConnectionDisconnected {
                notification: McpDisconnectNotification {
                    connection_id,
                    meta: None,
                },
            })
            .await
            .map_err(|_| agent_client_protocol_core::Error::internal_error())?;

        result
    }
}
