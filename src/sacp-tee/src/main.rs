//! sacp-tee - A debugging proxy that logs all ACP traffic to a file
//!
//! Usage:
//!   sacp-tee --log-file debug.log -- <command to run downstream agent>
//!   sacp-tee --log-file debug.log  # if downstream connects via stdio

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "sacp-tee")]
#[command(about = "A debugging proxy that logs all ACP traffic", long_about = None)]
struct Args {
    /// Path to the log file where messages will be recorded
    #[arg(short, long, default_value = "sacp-tee.log")]
    log_file: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    sacp_tee::run(args.log_file).await
}
