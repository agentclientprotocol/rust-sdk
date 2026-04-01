use agent_client_protocol_conductor::ConductorArgs;
use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    ConductorArgs::parse().main().await
}
