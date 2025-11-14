//! Integration test for nested conductors with proxy mode.
//!
//! This test verifies that:
//! 1. Conductors can be nested in proxy chains
//! 2. Inner conductor operates in proxy mode and forwards messages correctly
//! 3. Multiple arrow proxies work correctly through nested conductors
//! 4. The '>' prefix is applied multiple times (once per proxy)
//!
//! Chain structure:
//! test-editor -> outer_conductor -> inner_conductor -> eliza
//!                                    ├─ arrow_proxy1
//!                                    └─ arrow_proxy2
//!
//! Expected behavior:
//! - arrow_proxy1 adds first '>' to eliza's response: ">Hello..."
//! - arrow_proxy2 adds second '>' to that: ">>Hello..."
//! - Inner conductor operates in proxy mode, forwarding to eliza
//! - Outer conductor receives the ">>" prefixed response

use sacp::Component;
use sacp_conductor::conductor::Conductor;
use sacp_test::arrow_proxy::run_arrow_proxy;
use sacp_test::test_client::yolo_prompt;
use sacp_tokio::AcpAgent;
use std::str::FromStr;
use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Mock arrow proxy component for testing.
/// Runs the arrow proxy logic in-process instead of spawning a subprocess.
struct MockArrowProxy;

impl Component for MockArrowProxy {
    async fn serve(self, client: impl Component) -> Result<(), sacp::Error> {
        run_arrow_proxy(client).await
    }
}

/// Mock Eliza component for testing.
/// Runs the Eliza agent logic in-process instead of spawning a subprocess.
struct MockEliza;

impl Component for MockEliza {
    async fn serve(self, client: impl Component) -> Result<(), sacp::Error> {
        elizacp::run_elizacp(client).await
    }
}

/// Mock inner conductor component for testing.
/// Creates a nested conductor that runs in-process with mock arrow proxies.
struct MockInnerConductor {
    num_arrow_proxies: usize,
}

impl MockInnerConductor {
    fn new(num_arrow_proxies: usize) -> Self {
        Self { num_arrow_proxies }
    }
}

impl Component for MockInnerConductor {
    async fn serve(self, client: impl Component) -> Result<(), sacp::Error> {
        // Create mock arrow proxy components for the inner conductor
        // This conductor is ONLY proxies - no actual agent
        let mut components: Vec<sacp::DynComponent> = Vec::new();
        for _ in 0..self.num_arrow_proxies {
            components.push(sacp::DynComponent::new(MockArrowProxy));
        }

        Conductor::new("inner-conductor".to_string(), components, None)
            .run(client)
            .await
    }
}

#[tokio::test]
async fn test_nested_conductor_with_arrow_proxies() -> Result<(), sacp::Error> {
    // Create the nested component chain using mock components
    // Inner conductor will manage: arrow_proxy1 -> arrow_proxy2 -> eliza
    // Outer conductor will manage: inner_conductor only
    let inner_conductor = sacp::DynComponent::new(MockInnerConductor::new(2));

    // Create duplex streams for editor <-> conductor communication
    let (editor_write, conductor_read) = duplex(8192);
    let (conductor_write, editor_read) = duplex(8192);

    // Spawn the outer conductor with the inner conductor and eliza
    let conductor_handle = tokio::spawn(async move {
        Conductor::new(
            "outer-conductor".to_string(),
            vec![inner_conductor, sacp::DynComponent::new(MockEliza)],
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

        tracing::debug!(?result, "Received response from nested conductor chain");

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
        "Test completed successfully with double-arrow-prefixed response from nested conductor"
    );

    conductor_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_nested_conductor_with_external_arrow_proxies() -> Result<(), sacp::Error> {
    // Create the nested component chain using external processes
    // Inner conductor spawned as a separate process with two arrow proxies
    // Outer conductor manages: inner_conductor -> eliza (both as external processes)
    let inner_conductor = AcpAgent::from_str(
        "cargo run -p sacp-conductor -- agent 'cargo run -p sacp-test --example arrow_proxy' 'cargo run -p sacp-test --example arrow_proxy'",
    )?;
    let eliza = AcpAgent::from_str("cargo run -p elizacp")?;

    // Create duplex streams for editor <-> conductor communication
    let (editor_write, conductor_read) = duplex(8192);
    let (conductor_write, editor_read) = duplex(8192);

    // Spawn the outer conductor with the inner conductor and eliza as external processes
    let conductor_handle = tokio::spawn(async move {
        Conductor::new(
            "outer-conductor".to_string(),
            vec![inner_conductor, eliza],
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

        tracing::debug!(?result, "Received response from nested conductor chain");

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
        "Test completed successfully with double-arrow-prefixed response from nested conductor"
    );

    conductor_handle.abort();

    Ok(())
}
