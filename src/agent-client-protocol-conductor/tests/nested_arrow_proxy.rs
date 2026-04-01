//! Integration test for conductor with two arrow proxies in sequence.
//!
//! This test verifies that:
//! 1. Multiple arrow proxies work correctly in sequence
//! 2. The '>' prefix is applied multiple times (once per proxy)
//! 3. The full proxy chain works end-to-end
//!
//! Chain structure:
//! test-editor -> conductor -> arrow_proxy1 -> arrow_proxy2 -> test_agent
//!
//! Expected behavior:
//! - arrow_proxy2 adds first '>' to test_agent's response: ">Hello..."
//! - arrow_proxy1 adds second '>' to that: ">>Hello..."
//!
//! Run `just prep-tests` before running this test.

use agent_client_protocol_conductor::{ConductorImpl, McpBridgeMode, ProxiesAndAgent};
use agent_client_protocol_test::test_binaries::{arrow_proxy_example, testy};
use agent_client_protocol_test::testy::TestyCommand;
use agent_client_protocol_tokio::AcpAgent;
use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[tokio::test]
async fn test_conductor_with_two_external_arrow_proxies()
-> Result<(), agent_client_protocol_core::Error> {
    // Create the component chain: arrow_proxy1 -> arrow_proxy2 -> test_agent
    // Uses pre-built binaries to avoid cargo run races during `cargo test --all`
    let arrow_proxy1 = AcpAgent::from_args([arrow_proxy_example().to_string_lossy().to_string()])?;
    let arrow_proxy2 = AcpAgent::from_args([arrow_proxy_example().to_string_lossy().to_string()])?;
    let agent = testy();

    // Create duplex streams for editor <-> conductor communication
    let (editor_write, conductor_read) = duplex(8192);
    let (conductor_write, editor_read) = duplex(8192);

    // Spawn the conductor with three components
    let conductor_handle = tokio::spawn(async move {
        Box::pin(
            ConductorImpl::new_agent(
                "test-conductor".to_string(),
                ProxiesAndAgent::new(agent)
                    .proxy(arrow_proxy1)
                    .proxy(arrow_proxy2),
                McpBridgeMode::default(),
            )
            .run(agent_client_protocol_core::ByteStreams::new(
                conductor_write.compat_write(),
                conductor_read.compat(),
            )),
        )
        .await
    });

    // Wait for editor to complete and get the result
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        let result = Box::pin(yopo::prompt(
            agent_client_protocol_core::ByteStreams::new(
                editor_write.compat_write(),
                editor_read.compat(),
            ),
            TestyCommand::Greet.to_prompt(),
        ))
        .await?;

        expect_test::expect![[r#"
            ">>Hello, world!"
        "#]]
        .assert_debug_eq(&result);

        Ok::<String, agent_client_protocol_core::Error>(result)
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
