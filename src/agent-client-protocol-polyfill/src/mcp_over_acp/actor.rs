use agent_client_protocol::{
    ConnectTo, Dispatch, DynConnectTo, role::mcp, schema::McpDisconnectNotification,
};
use futures::{SinkExt as _, StreamExt as _, channel::mpsc};
use tracing::info;

use super::BridgeMessage;

/// Actor that bridges a single MCP connection between a local MCP client
/// and the ACP proxy chain.
#[derive(Debug)]
pub(crate) struct BridgeConnectionActor {
    /// How to connect to the MCP server (e.g., stdio or HTTP transport).
    transport: DynConnectTo<mcp::Client>,

    /// Sender for messages back to the polyfill's bridge responder loop.
    bridge_tx: mpsc::Sender<BridgeMessage>,

    /// Receiver for messages from the polyfill to forward to the MCP client.
    to_mcp_client_rx: mpsc::Receiver<Dispatch>,
}

impl BridgeConnectionActor {
    pub fn new(
        component: impl ConnectTo<mcp::Client>,
        bridge_tx: mpsc::Sender<BridgeMessage>,
        to_mcp_client_rx: mpsc::Receiver<Dispatch>,
    ) -> Self {
        Self {
            transport: DynConnectTo::new(component),
            bridge_tx,
            to_mcp_client_rx,
        }
    }

    pub async fn run(self, connection_id: String) -> Result<(), agent_client_protocol::Error> {
        info!(connection_id, "MCP bridge connected");

        let Self {
            transport,
            mut bridge_tx,
            to_mcp_client_rx,
        } = self;

        let result = mcp::Client
            .builder()
            .name(format!("mcp-client-to-polyfill({connection_id})"))
            .on_receive_dispatch(
                {
                    let mut bridge_tx = bridge_tx.clone();
                    let connection_id = connection_id.clone();
                    async move |message: Dispatch, _cx| {
                        bridge_tx
                            .send(BridgeMessage::ClientToServer {
                                connection_id: connection_id.clone(),
                                message,
                            })
                            .await
                            .map_err(|_| agent_client_protocol::Error::internal_error())
                    }
                },
                agent_client_protocol::on_receive_dispatch!(),
            )
            .connect_with(transport, async move |mcp_connection_to_client| {
                let mut to_mcp_client_rx = to_mcp_client_rx;
                while let Some(message) = to_mcp_client_rx.next().await {
                    mcp_connection_to_client.send_proxied_message(message)?;
                }
                Ok(())
            })
            .await;

        bridge_tx
            .send(BridgeMessage::Disconnected {
                notification: McpDisconnectNotification {
                    connection_id,
                    meta: None,
                },
            })
            .await
            .map_err(|_| agent_client_protocol::Error::internal_error())?;

        result
    }
}
