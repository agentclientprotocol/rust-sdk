//! Integration tests for MCP tool routing through proxy components.
//!
//! These tests verify that:
//! 1. Proxy components can provide MCP tools
//! 2. Agent components can discover and invoke those tools
//! 3. Tool invocations route correctly through the proxy

mod mcp_integration;

use expect_test::expect;
use futures::{SinkExt, StreamExt, channel::mpsc};
use sacp::JrHandlerChain;
use sacp::schema::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, SessionNotification,
    TextContent,
};
use sacp_conductor::conductor::Conductor;

use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Test helper to receive a JSON-RPC response
async fn recv<R: sacp::JrResponsePayload + Send>(
    response: sacp::JrResponse<R>,
) -> Result<R, sacp::Error> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    response.await_when_result_received(async move |result| {
        tx.send(result).map_err(|_| sacp::Error::internal_error())
    })?;
    rx.await.map_err(|_| sacp::Error::internal_error())?
}

fn conductor_command() -> Vec<String> {
    vec![
        "cargo".to_string(),
        "run".to_string(),
        "-p".to_string(),
        "sacp-conductor".to_string(),
        "--".to_string(),
    ]
}

async fn run_test_with_components(
    components: Vec<sacp::DynComponent>,
    editor_task: impl AsyncFnOnce(sacp::JrConnectionCx) -> Result<(), sacp::Error>,
) -> Result<(), sacp::Error> {
    // Set up editor <-> conductor communication
    let (editor_out, conductor_in) = duplex(1024);
    let (conductor_out, editor_in) = duplex(1024);

    let transport = sacp::ByteStreams::new(editor_out.compat_write(), editor_in.compat());

    JrHandlerChain::new()
        .name("editor-to-connector")
        .with_spawned(|_cx| async move {
            Conductor::new(
                "conductor".to_string(),
                components,
                Some(conductor_command()),
            )
            .run(sacp::ByteStreams::new(
                conductor_out.compat_write(),
                conductor_in.compat(),
            ))
            .await
        })
        .with_client(transport, editor_task)
        .await
}

#[tokio::test]
async fn test_proxy_provides_mcp_tools() -> Result<(), sacp::Error> {
    run_test_with_components(
        vec![
            mcp_integration::proxy::create(),
            mcp_integration::agent::create(),
        ],
        async |editor_cx| {
            // Send initialization request
            let init_response = recv(editor_cx.send_request(InitializeRequest {
                protocol_version: Default::default(),
                client_capabilities: Default::default(),
                meta: None,
                client_info: None,
            }))
            .await;

            assert!(
                init_response.is_ok(),
                "Initialize should succeed: {:?}",
                init_response
            );

            // Send session/new request
            let session_response = recv(editor_cx.send_request(NewSessionRequest {
                cwd: Default::default(),
                mcp_servers: vec![],
                meta: None,
            }))
            .await;

            assert!(
                session_response.is_ok(),
                "Session/new should succeed: {:?}",
                session_response
            );

            let session = session_response.unwrap();
            assert_eq!(&*session.session_id.0, "test-session-123");

            Ok(())
        },
    )
    .await?;

    Ok(())
}

#[tokio::test]
async fn test_agent_handles_prompt() -> Result<(), sacp::Error> {
    // Create channel to collect log events
    let (mut log_tx, mut log_rx) = mpsc::unbounded();

    // Set up editor <-> conductor communication with notification handling
    let (editor, conductor) = duplex(1024);
    let (editor_in, editor_out) = tokio::io::split(editor);
    let (conductor_in, conductor_out) = tokio::io::split(conductor);

    let transport = sacp::ByteStreams::new(editor_out.compat_write(), editor_in.compat());

    JrHandlerChain::new()
        .name("editor-to-connector")
        .on_receive_notification({
            let mut log_tx = log_tx.clone();
            async move |notification: SessionNotification, _cx| {
                // Log the notification in debug format
                log_tx
                    .send(format!("{notification:?}"))
                    .await
                    .map_err(|_| sacp::Error::internal_error())
            }
        })
        .with_spawned(|_cx| async move {
            Conductor::new(
                "mcp-integration-conductor".to_string(),
                vec![
                    mcp_integration::proxy::create(),
                    mcp_integration::agent::create(),
                ],
                Some(conductor_command()),
            )
            .run(sacp::ByteStreams::new(
                conductor_out.compat_write(),
                conductor_in.compat(),
            ))
            .await
        })
        .with_client(transport, async |editor_cx| {
            // Initialize
            recv(editor_cx.send_request(InitializeRequest {
                protocol_version: Default::default(),
                client_capabilities: Default::default(),
                meta: None,
                client_info: None,
            }))
            .await?;

            // Create session
            let session = recv(editor_cx.send_request(NewSessionRequest {
                cwd: Default::default(),
                mcp_servers: vec![],
                meta: None,
            }))
            .await?;

            tracing::debug!(session_id = %session.session_id.0, "Session created");

            // Send a prompt
            let prompt_response = recv(editor_cx.send_request(PromptRequest {
                session_id: session.session_id.clone(),
                prompt: vec![ContentBlock::Text(TextContent {
                    annotations: None,
                    text: "Hello agent!".to_string(),
                    meta: None,
                })],
                meta: None,
            }))
            .await?;

            // Log the response
            log_tx
                .send(format!("{prompt_response:?}"))
                .await
                .map_err(|_| sacp::Error::internal_error())?;

            Ok(())
        })
        .await?;

    // Drop the sender and collect all log entries
    drop(log_tx);
    let mut log_entries = Vec::new();
    while let Some(entry) = log_rx.next().await {
        log_entries.push(entry);
    }

    // Verify the output
    expect![[r#"
        [
            "SessionNotification { session_id: SessionId(\"test-session-123\"), update: AgentMessageChunk(ContentChunk { content: Text(TextContent { annotations: None, text: \"Hello. I will now use the MCP tool\", meta: None }), meta: None }), meta: None }",
            "SessionNotification { session_id: SessionId(\"test-session-123\"), update: AgentMessageChunk(ContentChunk { content: Text(TextContent { annotations: None, text: \"MCP tool result: CallToolResult { content: [Annotated { raw: Text(RawTextContent { text: \\\"Echo: Hello from the agent!\\\", meta: None }), annotations: None }], structured_content: None, is_error: Some(false), meta: None }\", meta: None }), meta: None }), meta: None }",
            "PromptResponse { stop_reason: EndTurn, meta: None }",
        ]
    "#]]
    .assert_debug_eq(&log_entries);

    Ok(())
}
