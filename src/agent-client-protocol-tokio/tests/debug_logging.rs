//! Integration test for AcpAgent debug logging

use agent_client_protocol::schema::InitializeRequest;
use agent_client_protocol::{Client, ConnectTo};
use agent_client_protocol_test::test_binaries::testy;
use agent_client_protocol_tokio::LineDirection;
use std::sync::{Arc, Mutex};

/// Test helper to receive a JSON-RPC response
async fn recv<T: agent_client_protocol::JsonRpcResponse + Send>(
    response: agent_client_protocol::SentRequest<T>,
) -> Result<T, agent_client_protocol::Error> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    response.on_receiving_result(async move |result| {
        tx.send(result)
            .map_err(|_| agent_client_protocol::Error::internal_error())
    })?;
    rx.await
        .map_err(|_| agent_client_protocol::Error::internal_error())?
}

#[tokio::test]
async fn test_acp_agent_debug_callback() -> Result<(), Box<dyn std::error::Error>> {
    use tokio::io::duplex;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    // Collect debug output
    #[derive(Debug, Clone, Default)]
    struct DebugLog {
        lines: Arc<Mutex<Vec<(String, LineDirection)>>>,
    }

    impl DebugLog {
        fn log(&self, line: &str, direction: LineDirection) {
            self.lines
                .lock()
                .unwrap()
                .push((line.to_string(), direction));
        }

        fn get_lines(&self) -> Vec<(String, LineDirection)> {
            self.lines.lock().unwrap().clone()
        }
    }

    let debug_log = DebugLog::default();

    // Create an agent that runs test-agent
    let agent = testy().with_debug({
        let debug_log = debug_log.clone();
        move |line, direction| {
            debug_log.log(line, direction);
        }
    });

    // Set up client <-> agent communication
    let (client_out, agent_in) = duplex(1024);
    let (agent_out, client_in) = duplex(1024);

    let transport =
        agent_client_protocol::ByteStreams::new(client_out.compat_write(), client_in.compat());

    Client
        .builder()
        .name("test-client")
        .with_spawned(|_cx| async move {
            ConnectTo::<Client>::connect_to(
                agent,
                agent_client_protocol::ByteStreams::new(
                    agent_out.compat_write(),
                    agent_in.compat(),
                ),
            )
            .await
        })
        .connect_with(transport, async |connection_to_client| {
            // Send an initialize request
            let _init_response = recv(connection_to_client.send_request(InitializeRequest::new(
                agent_client_protocol::schema::ProtocolVersion::LATEST,
            )))
            .await?;

            Ok(())
        })
        .await?;

    // Verify debug output was captured
    let logged_lines = debug_log.get_lines();

    // Should have at least some stdin and stdout lines
    let stdin_count = logged_lines
        .iter()
        .filter(|(_, dir)| *dir == LineDirection::Stdin)
        .count();
    let stdout_count = logged_lines
        .iter()
        .filter(|(_, dir)| *dir == LineDirection::Stdout)
        .count();

    assert!(
        stdin_count > 0,
        "Expected at least one stdin line, got {stdin_count}"
    );
    assert!(
        stdout_count > 0,
        "Expected at least one stdout line, got {stdout_count}"
    );

    // Check that we logged the initialize request (contains "initialize" method)
    let has_initialize_request = logged_lines.iter().any(|(line, dir)| {
        *dir == LineDirection::Stdin && line.contains("\"method\":\"initialize\"")
    });
    assert!(
        has_initialize_request,
        "Expected to find initialize request in debug log"
    );

    // Check that we logged the initialize response (contains result field)
    let has_initialize_response = logged_lines
        .iter()
        .any(|(line, dir)| *dir == LineDirection::Stdout && line.contains("\"result\""));
    assert!(
        has_initialize_response,
        "Expected to find initialize response in debug log"
    );

    Ok(())
}
