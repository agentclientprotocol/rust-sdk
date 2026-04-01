//! Integration test for conductor with an empty conductor and test agent.
//!
//! This test verifies that:
//! 1. Conductor can orchestrate a chain with an empty conductor as a proxy + test agent
//! 2. Empty conductor (with no components) correctly acts as a passthrough proxy
//! 3. Messages flow correctly through the empty conductor to the agent
//! 4. The full chain works end-to-end

use agent_client_protocol_conductor::{ConductorImpl, McpBridgeMode, ProxiesAndAgent};
use agent_client_protocol_core::{Conductor, ConnectTo, Proxy};
use agent_client_protocol_test::testy::{Testy, TestyCommand};
use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Mock empty conductor component for testing.
/// Creates a nested conductor with no components that acts as a passthrough proxy.
struct MockEmptyConductor;

impl ConnectTo<Conductor> for MockEmptyConductor {
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol_core::Error> {
        // Create an empty conductor with no components - it should act as a passthrough
        let empty_components: Vec<agent_client_protocol_core::DynConnectTo<Conductor>> = vec![];
        ConnectTo::<Conductor>::connect_to(
            ConductorImpl::new_proxy(
                "empty-conductor".to_string(),
                empty_components,
                McpBridgeMode::default(),
            ),
            client,
        )
        .await
    }
}

#[tokio::test]
async fn test_conductor_with_empty_conductor_and_test_agent()
-> Result<(), agent_client_protocol_core::Error> {
    // Initialize tracing for debugging
    drop(
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("trace")),
            )
            .with_test_writer()
            .try_init(),
    );
    // Create duplex streams for editor <-> conductor communication
    let (editor_write, conductor_read) = duplex(8192);
    let (conductor_write, editor_read) = duplex(8192);

    // Spawn the conductor
    let conductor_handle = tokio::spawn(async move {
        Box::pin(
            ConductorImpl::new_agent(
                "outer-conductor".to_string(),
                ProxiesAndAgent::new(Testy::new()).proxy(MockEmptyConductor),
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

        tracing::debug!(?result, "Received response from empty conductor chain");

        // Empty conductor should not modify the response
        expect_test::expect![[r#"
            "Hello, world!"
        "#]]
        .assert_debug_eq(&result);

        Ok::<String, agent_client_protocol_core::Error>(result)
    })
    .await
    .expect("Test timed out")
    .expect("Editor failed");

    tracing::info!(
        ?result,
        "Test completed successfully with response from empty conductor chain"
    );

    conductor_handle.abort();

    Ok(())
}
