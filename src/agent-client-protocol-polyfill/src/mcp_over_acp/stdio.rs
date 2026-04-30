//! Stdio-based MCP bridge transport.

use agent_client_protocol::Dispatch;
use futures::{SinkExt, channel::mpsc};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

use super::{BridgeConnection, BridgeMessage, actor::BridgeConnectionActor};

/// Runs the stdio bridge TCP listener, accepting connections and creating bridge actors.
pub async fn run_tcp_listener(
    tcp_listener: TcpListener,
    acp_id: String,
    mut bridge_tx: mpsc::Sender<BridgeMessage>,
) -> Result<(), agent_client_protocol::Error> {
    loop {
        let (stream, _addr) = tcp_listener
            .accept()
            .await
            .map_err(agent_client_protocol::Error::into_internal_error)?;

        let (to_mcp_client_tx, to_mcp_client_rx) = mpsc::channel(128);

        bridge_tx
            .send(BridgeMessage::ConnectionReceived {
                acp_id: acp_id.clone(),
                actor: make_stdio_actor(stream, bridge_tx.clone(), to_mcp_client_rx),
                connection: BridgeConnection::new(to_mcp_client_tx),
            })
            .await
            .map_err(|_| agent_client_protocol::Error::internal_error())?;
    }
}

fn make_stdio_actor(
    stream: TcpStream,
    bridge_tx: mpsc::Sender<BridgeMessage>,
    to_mcp_client_rx: mpsc::Receiver<Dispatch>,
) -> BridgeConnectionActor {
    let (read_half, write_half) = stream.into_split();
    let transport =
        agent_client_protocol::ByteStreams::new(write_half.compat_write(), read_half.compat());
    BridgeConnectionActor::new(transport, bridge_tx, to_mcp_client_rx)
}
