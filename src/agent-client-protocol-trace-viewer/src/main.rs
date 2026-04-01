//! Agent Client Protocol Trace Viewer
//!
//! Interactive sequence diagram viewer for ACP trace files.
//!
//! Usage:
//! ```bash
//! agent-client-protocol-trace-viewer ./trace.jsons
//! ```
//!
//! This starts a local HTTP server and opens a browser to view the trace
//! as an interactive sequence diagram.

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Interactive sequence diagram viewer for ACP trace files"
)]
struct Args {
    /// Path to the trace file (.jsons)
    trace_file: PathBuf,

    /// Port to serve on (default: auto-select)
    #[arg(short, long)]
    port: Option<u16>,

    /// Don't open browser automatically
    #[arg(long)]
    no_open: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Verify trace file exists
    if !args.trace_file.exists() {
        anyhow::bail!("Trace file not found: {}", args.trace_file.display());
    }

    let config = agent_client_protocol_trace_viewer::TraceViewerConfig {
        port: args.port.unwrap_or(0),
        open_browser: !args.no_open,
    };

    agent_client_protocol_trace_viewer::serve_file(args.trace_file, config).await
}
