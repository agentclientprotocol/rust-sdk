use clap::Parser;
use agent_client_protocol_conductor::ConductorArgs;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    ConductorArgs::parse().main().await
}
