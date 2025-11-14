//! Integration test for conductor with two arrow proxies in sequence.
//!
//! This test verifies that:
//! 1. Multiple arrow proxies work correctly in sequence
//! 2. The '>' prefix is applied multiple times (once per proxy)
//! 3. The full proxy chain works end-to-end
//!
//! Chain structure:
//! test-editor -> conductor -> arrow_proxy1 -> arrow_proxy2 -> eliza
//!
//! Expected behavior:
//! - arrow_proxy2 adds first '>' to eliza's response: ">Hello..."
//! - arrow_proxy1 adds second '>' to that: ">>Hello..."

use sacp_conductor::conductor::Conductor;
use sacp_test::test_client::yolo_prompt;
use sacp_tokio::AcpAgent;
use std::str::FromStr;
use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[tokio::test]
async fn test_conductor_with_two_external_arrow_proxies() -> Result<(), sacp::Error> {
    // Create the component chain: arrow_proxy1 -> arrow_proxy2 -> eliza
    let arrow_proxy1 = AcpAgent::from_str("cargo run -p sacp-test --example arrow_proxy")?;
    let arrow_proxy2 = AcpAgent::from_str("cargo run -p sacp-test --example arrow_proxy")?;
    let eliza = AcpAgent::from_str("cargo run -p elizacp")?;

    // Create duplex streams for editor <-> conductor communication
    let (editor_write, conductor_read) = duplex(8192);
    let (conductor_write, editor_read) = duplex(8192);

    // Spawn the conductor with three components
    let conductor_handle = tokio::spawn(async move {
        Conductor::new(
            "test-conductor".to_string(),
            vec![arrow_proxy1, arrow_proxy2, eliza],
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

        expect_test::expect![[r#"
            ">>Hello. How are you feeling today?"
        "#]]
        .assert_debug_eq(&result);

        Ok::<String, sacp::Error>(result)
    })
    .await
    .expect("Test timed out")
    .expect("Editor failed");

    tracing::info!(
        ?result,
        "Test completed successfully with double-arrow-prefixed response"
    );

    conductor_handle.abort();

    Ok(())
}
