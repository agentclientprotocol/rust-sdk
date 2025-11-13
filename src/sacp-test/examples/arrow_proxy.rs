//! Arrow proxy example that can be run as a standalone process.
//!
//! This adds '>' prefix to session updates for testing conductor proxy chains.

use sacp_test::arrow_proxy::run_arrow_proxy;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing for debugging
    tracing_subscriber::fmt()
        .with_target(true)
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("Arrow proxy starting");

    // Create connection over stdio with compat layer for tokio/futures interop
    let stdin = tokio::io::stdin().compat();
    let stdout = tokio::io::stdout().compat_write();

    // Run the arrow proxy
    run_arrow_proxy(sacp::ByteStreams::new(stdout, stdin)).await?;

    Ok(())
}
