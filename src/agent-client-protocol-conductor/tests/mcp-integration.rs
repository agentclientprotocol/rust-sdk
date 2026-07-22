//! Integration tests for MCP tool routing through proxy components.
//!
//! These tests verify that:
//! 1. Proxy components can provide MCP tools
//! 2. Agent components can discover and invoke those tools
//! 3. Tool invocations route correctly through the proxy

mod mcp_integration;

use agent_client_protocol::Agent;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, SessionNotification,
    TextContent,
};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use agent_client_protocol_polyfill::mcp_over_acp::McpOverAcpPolyfill;
use agent_client_protocol_test::testy::{Testy, TestyCommand};
use futures::{SinkExt, StreamExt, channel::mpsc};

use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

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
async fn test_agent_handles_prompt() -> Result<(), agent_client_protocol::Error> {
    // Initialize tracing for debug output
    drop(
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init(),
    );

    // Create channel to collect log events
    let (mut log_tx, mut log_rx) = mpsc::unbounded();

    // Create duplex streams for client <-> conductor communication
    let (client_write, conductor_read) = duplex(8192);
    let (conductor_write, client_read) = duplex(8192);

    // Spawn the conductor in a background task
    let conductor_handle = tokio::spawn(async move {
        ConductorImpl::new_agent(
            "mcp-integration-conductor".to_string(),
            ProxiesAndAgent::new(Testy::new())
                .proxy(mcp_integration::proxy::ProxyComponent)
                .proxy(McpOverAcpPolyfill::http()),
        )
        .run(agent_client_protocol::ByteStreams::new(
            conductor_write.compat_write(),
            conductor_read.compat(),
        ))
        .await
    });

    // Run the client
    let result = agent_client_protocol::Client
        .builder()
        .name("editor-to-connector")
        .on_receive_notification(
            {
                let mut log_tx = log_tx.clone();
                async move |notification: SessionNotification,
                            _cx: agent_client_protocol::ConnectionTo<Agent>| {
                    // Log the notification in debug format
                    log_tx
                        .send(format!("{notification:?}"))
                        .await
                        .map_err(|_| agent_client_protocol::Error::internal_error())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            agent_client_protocol::ByteStreams::new(
                client_write.compat_write(),
                client_read.compat(),
            ),
            async |connection_to_editor| {
                // Initialize
                recv(
                    connection_to_editor.send_request(InitializeRequest::new(ProtocolVersion::V1)),
                )
                .await?;

                // Create session
                let session = recv(
                    connection_to_editor
                        .send_request(NewSessionRequest::new(std::path::PathBuf::from("/"))),
                )
                .await?;

                tracing::debug!(session_id = %session.session_id.0, "Session created");

                // Send a prompt to call the echo tool
                let prompt_response = recv(connection_to_editor.send_request(PromptRequest::new(
                    session.session_id.clone(),
                    vec![ContentBlock::Text(TextContent::new(TestyCommand::CallTool {
                        server: "test".to_string(),
                        tool: "echo".to_string(),
                        params: serde_json::json!({"message": "Hello from the test!"}),
                    }.to_prompt()))],
                )))
                .await?;

                // Log the response
                log_tx
                    .send(format!("{prompt_response:?}"))
                    .await
                    .map_err(|_| agent_client_protocol::Error::internal_error())?;

                Ok(())
            },
        )
        .await;

    conductor_handle.abort();
    result?;

    // Drop the sender and collect all log entries
    drop(log_tx);
    let mut log_entries = Vec::new();
    while let Some(entry) = log_rx.next().await {
        log_entries.push(entry);
    }

    // Verify we got a successful tool call response
    // The session ID is opaque, so check the observable tool result pattern.
    assert_eq!(log_entries.len(), 2, "Expected notification + response");
    assert!(
        log_entries[0].contains("OK: CallToolResult"),
        "Expected successful tool call, got: {}",
        log_entries[0]
    );
    assert!(
        log_entries[0].contains("Echo: Hello from the test!"),
        "Expected echo result, got: {}",
        log_entries[0]
    );
    assert!(
        log_entries[1].contains("PromptResponse"),
        "Expected prompt response, got: {}",
        log_entries[1]
    );

    Ok(())
}
