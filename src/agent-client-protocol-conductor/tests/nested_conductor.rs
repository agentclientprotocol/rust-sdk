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
//!
//! Run `just prep-tests` before running these tests.

use agent_client_protocol_core::{Conductor, ConnectTo, DynConnectTo};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use agent_client_protocol_test::arrow_proxy::run_arrow_proxy;
use agent_client_protocol_test::test_binaries::{arrow_proxy_example, conductor_binary, testy};
use agent_client_protocol_test::testy::{Testy, TestyCommand};
use agent_client_protocol_tokio::AcpAgent;
use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Mock arrow proxy component for testing.
/// Runs the arrow proxy logic in-process instead of spawning a subprocess.
struct MockArrowProxy;

impl ConnectTo<Conductor> for MockArrowProxy {
    async fn connect_to(self, client: impl ConnectTo<agent_client_protocol_core::Proxy>) -> Result<(), agent_client_protocol_core::Error> {
        run_arrow_proxy(client).await
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

impl ConnectTo<Conductor> for MockInnerConductor {
    async fn connect_to(self, client: impl ConnectTo<agent_client_protocol_core::Proxy>) -> Result<(), agent_client_protocol_core::Error> {
        // Create mock arrow proxy components for the inner conductor
        // This conductor is ONLY proxies - no actual agent
        // Use Serve::serve instead of .run() to get the Serve<Conductor> impl
        let mut components: Vec<DynConnectTo<Conductor>> = Vec::new();
        for _ in 0..self.num_arrow_proxies {
            components.push(DynConnectTo::new(MockArrowProxy));
        }

        ConnectTo::<Conductor>::connect_to(
            agent_client_protocol_conductor::ConductorImpl::new_proxy(
                "inner-conductor".to_string(),
                components,
                Default::default(),
            ),
            client,
        )
        .await
    }
}

#[tokio::test]
async fn test_nested_conductor_with_arrow_proxies() -> Result<(), agent_client_protocol_core::Error> {
    // Create the nested component chain using mock components
    // Inner conductor will manage: arrow_proxy1 -> arrow_proxy2 -> eliza
    // Outer conductor will manage: inner_conductor only

    // Create duplex streams for editor <-> conductor communication
    let (editor_write, conductor_read) = duplex(8192);
    let (conductor_write, editor_read) = duplex(8192);

    // Spawn the outer conductor with the inner conductor and eliza
    let conductor_handle = tokio::spawn(async move {
        ConductorImpl::new_agent(
            "outer-conductor".to_string(),
            ProxiesAndAgent::new(Testy::new()).proxy(MockInnerConductor::new(2)),
            Default::default(),
        )
        .run(agent_client_protocol_core::ByteStreams::new(
            conductor_write.compat_write(),
            conductor_read.compat(),
        ))
        .await
    });

    // Wait for editor to complete and get the result
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        let result = agent_client_protocol_yopo::prompt(
            agent_client_protocol_core::ByteStreams::new(editor_write.compat_write(), editor_read.compat()),
            TestyCommand::Greet.to_prompt(),
        )
        .await?;

        tracing::debug!(?result, "Received response from nested conductor chain");

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
        "Test completed successfully with double-arrow-prefixed response from nested conductor"
    );

    conductor_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_nested_conductor_with_external_arrow_proxies() -> Result<(), agent_client_protocol_core::Error> {
    // Create the nested component chain using external processes
    // Inner conductor spawned as a separate process with two arrow proxies
    // Outer conductor manages: inner_conductor -> test agent (both as external processes)
    // Uses pre-built binaries to avoid cargo run races during `cargo test --all`
    let conductor_path = conductor_binary().to_string_lossy().to_string();
    let arrow_proxy_path = arrow_proxy_example().to_string_lossy().to_string();
    let inner_conductor = AcpAgent::from_args([
        &conductor_path,
        "proxy",
        &arrow_proxy_path,
        &arrow_proxy_path,
    ])?;
    let agent = testy();

    // Create duplex streams for editor <-> conductor communication
    let (editor_write, conductor_read) = duplex(8192);
    let (conductor_write, editor_read) = duplex(8192);

    // Spawn the outer conductor with the inner conductor and eliza as external processes
    let conductor_handle = tokio::spawn(async move {
        ConductorImpl::new_agent(
            "outer-conductor".to_string(),
            ProxiesAndAgent::new(agent).proxy(inner_conductor),
            Default::default(),
        )
        .run(agent_client_protocol_core::ByteStreams::new(
            conductor_write.compat_write(),
            conductor_read.compat(),
        ))
        .await
    });

    // Wait for editor to complete and get the result
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        let result = agent_client_protocol_yopo::prompt(
            agent_client_protocol_core::ByteStreams::new(editor_write.compat_write(), editor_read.compat()),
            TestyCommand::Greet.to_prompt(),
        )
        .await?;

        tracing::debug!(?result, "Received response from nested conductor chain");

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
        "Test completed successfully with double-arrow-prefixed response from nested conductor"
    );

    conductor_handle.abort();

    Ok(())
}
