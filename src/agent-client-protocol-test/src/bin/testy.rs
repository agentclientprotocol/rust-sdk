use agent_client_protocol::ConnectTo;
use agent_client_protocol_test::testy::Testy;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    #[cfg(feature = "unstable_protocol_v2")]
    Testy::new()
        .protocol_router()
        .connect_to(agent_client_protocol::Stdio::new())
        .await?;

    #[cfg(not(feature = "unstable_protocol_v2"))]
    Testy::new()
        .connect_to(agent_client_protocol::Stdio::new())
        .await?;

    Ok(())
}
