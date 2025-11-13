//! Minimal "do nothing" proxy example
//!
//! This is the simplest possible proxy - it just forwards all messages unchanged.
//! Use this as a starting point for building your own proxy.
//!
//! Run with:
//! ```bash
//! cargo run --example minimal
//! ```

use sacp::JrHandlerChain;
use sacp_proxy::{AcpProxyExt, McpServiceRegistry};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing for debugging
    tracing_subscriber::fmt()
        .with_target(true)
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("Minimal proxy starting");

    // Set up the proxy connection
    JrHandlerChain::new()
        .name("minimal-proxy")
        // Empty MCP registry - no tools provided
        .provide_mcp(McpServiceRegistry::default())
        // Enable proxy mode (handles capability handshake and message routing)
        .proxy()
        // Start serving
        .connect_to(sacp::ByteStreams::new(
            tokio::io::stdout().compat_write(),
            tokio::io::stdin().compat(),
        ))?
        .serve()
        .await?;

    Ok(())
}
