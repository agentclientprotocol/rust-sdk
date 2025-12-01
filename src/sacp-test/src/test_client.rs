//! Test client helper for conductor integration tests.
//!
//! Provides a simple helper function to connect to an agent, send a prompt,
//! and collect all session updates into a string.

use sacp::{
    JrHandlerChain,
    schema::{
        ContentBlock, InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse,
        PromptRequest, PromptResponse, ProtocolVersion, SessionNotification, SessionUpdate,
    },
};

/// Connect to an agent, send a prompt, and collect all session update text.
///
/// This helper:
/// - Connects to the agent via the provided streams
/// - Initializes the connection
/// - Creates a new session
/// - Sends the prompt
/// - Collects all text from `AgentMessageChunk` session updates
/// - Returns the concatenated result
///
/// # Example
///
/// ```ignore
/// let (write, read) = create_streams_to_agent();
/// let result = yolo_prompt(write, read, "Hello").await?;
/// assert!(result.contains("response text"));
/// ```
pub async fn yolo_prompt<OB, IB>(
    outgoing: OB,
    incoming: IB,
    prompt: &str,
) -> Result<String, sacp::Error>
where
    OB: futures::AsyncWrite + Send + 'static,
    IB: futures::AsyncRead + Send + 'static,
{
    let mut collected_text = String::new();

    JrHandlerChain::new()
        .name("test-client")
        .on_receive_notification(async |notif: SessionNotification, _cx| {
            // Collect text from AgentMessageChunk updates
            if let SessionUpdate::AgentMessageChunk(chunk) = &notif.update
                && let ContentBlock::Text(text_content) = &chunk.content
            {
                collected_text.push_str(&text_content.text);
            }
            Ok(())
        })
        .connect_to(sacp::ByteStreams::new(outgoing, incoming))?
        .with_client(async move |cx| {
            // Initialize
            let InitializeResponse { .. } = cx
                .send_request(InitializeRequest::new(ProtocolVersion::LATEST))
                .block_task()
                .await?;

            // Create session
            let NewSessionResponse { session_id, .. } = cx
                .send_request(NewSessionRequest::new(
                    std::env::current_dir().unwrap_or_default(),
                ))
                .block_task()
                .await?;

            // Send prompt
            let PromptResponse { stop_reason, .. } = cx
                .send_request(PromptRequest::new(session_id, vec![prompt.into()]))
                .block_task()
                .await?;

            match stop_reason {
                sacp::schema::StopReason::EndTurn => Ok(()),
                _ => Err(sacp::util::internal_error(format!(
                    "prompt stopped early: {stop_reason:?}"
                ))),
            }
        })
        .await?;

    Ok(collected_text)
}
