use agent_client_protocol_core::ConnectTo;
use agent_client_protocol_test::testy::Testy;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    Testy::new()
        .connect_to(agent_client_protocol_tokio::Stdio::new())
        .await?;
    Ok(())
}
