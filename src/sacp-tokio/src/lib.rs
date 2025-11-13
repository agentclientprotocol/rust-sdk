//! Tokio-based utilities for SACP
//!
//! This crate provides higher-level functionality for working with SACP
//! that requires the Tokio async runtime, such as spawning agent processes
//! and creating connections.

mod acp_agent;

pub use acp_agent::AcpAgent;
use sacp::{ByteStreams, Component};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

pub struct Stdio;

impl Component for Stdio {
    async fn serve(self, client: impl Component) -> Result<(), sacp::Error> {
        ByteStreams::new(
            tokio::io::stdout().compat_write(),
            tokio::io::stdin().compat(),
        )
        .serve(client)
        .await
    }
}
