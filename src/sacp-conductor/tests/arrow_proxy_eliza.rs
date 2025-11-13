//! Integration test for conductor with arrow proxy and eliza agent.
//!
//! This test verifies that:
//! 1. Conductor can orchestrate a proxy chain with arrow proxy + eliza
//! 2. Session updates from eliza get the '>' prefix from arrow proxy
//! 3. The full proxy chain works end-to-end

use sacp_conductor::conductor::Conductor;
use sacp_test::test_client::yolo_prompt;
use sacp_tokio::AcpAgent;
use std::str::FromStr;
use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[tokio::test]
async fn test_conductor_with_arrow_proxy_and_eliza() -> Result<(), sacp::Error> {
    // Create the component chain: arrow_proxy -> eliza
    let arrow_proxy_agent = AcpAgent::from_str("cargo run -p sacp-test --example arrow_proxy")?;
    let eliza_agent = AcpAgent::from_str("cargo run -p elizacp")?;

    // Create duplex streams for editor <-> conductor communication
    let (editor_write, conductor_read) = duplex(8192);
    let (conductor_write, editor_read) = duplex(8192);

    // Spawn the conductor
    let conductor_handle = tokio::spawn(async move {
        Conductor::new(
            "conductor".to_string(),
            vec![arrow_proxy_agent, eliza_agent],
            None,
        )
        .run(sacp::ByteStreams::new(
            conductor_write.compat_write(),
            conductor_read.compat(),
        ))
        .await
    });

    // Wait for editor to complete and get the result
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        let result =
            yolo_prompt(editor_write.compat_write(), editor_read.compat(), "Hello").await?;

        tracing::debug!(?result, "Received response from arrow proxy chain");

        assert!(
            result.starts_with('>'),
            "Expected response to start with '>' from arrow proxy, got: {}",
            result
        );

        Ok::<String, sacp::Error>(result)
    })
    .await
    .expect("Test timed out")
    .expect("Editor failed");

    tracing::info!(
        ?result,
        "Test completed successfully with arrow-prefixed response"
    );

    conductor_handle.abort();

    Ok(())
}
