use std::path::PathBuf;

use clap::Parser;
use sacp_conductor::ConductorArgs;
use tracing::Instrument;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let pid = std::process::id();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());

    // Check for SYMPOSIUM_LOG environment variable
    if let Ok(log_level) = std::env::var("SYMPOSIUM_LOG") {
        // Set up file logging to ~/.symposium/logs.$DATE
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));

        let log_dir = home.join(".symposium");
        std::fs::create_dir_all(&log_dir)?;

        let file_appender = tracing_appender::rolling::daily(log_dir, "logs");

        tracing_subscriber::registry()
            .with(EnvFilter::new(&log_level))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_target(true)
                    .with_span_events(
                        tracing_subscriber::fmt::format::FmtSpan::NEW
                            | tracing_subscriber::fmt::format::FmtSpan::CLOSE,
                    )
                    .with_writer(file_appender),
            )
            .init();

        tracing::info!(
            pid = %pid,
            cwd = %cwd,
            level = %log_level,
            "Conductor starting with file logging"
        );
    } else {
        // Initialize tracing with env filter support (RUST_LOG=debug, etc.)
        // Important: Always write to stderr to avoid interfering with stdio protocols
        tracing_subscriber::registry()
            .with(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new("conductor=info")),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .with_target(true)
                    .with_writer(std::io::stderr),
            )
            .init();

        tracing::info!(pid = %pid, cwd = %cwd, "Conductor starting");
    }

    ConductorArgs::parse()
        .run()
        .instrument(tracing::info_span!("conductor", pid = %pid, cwd = %cwd))
        .await
        .map_err(|err| anyhow::anyhow!("{err}"))
}
