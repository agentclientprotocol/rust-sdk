//! YOPO (You Only Prompt Once) - A simple library for testing ACP agents
//!
//! Provides a convenient API for running one-shot prompts against ACP components.

use agent_client_protocol::schema::{
    AudioContent, ContentBlock, EmbeddedResourceResource, ImageContent, InitializeRequest,
    ProtocolVersion, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, TextContent,
};
use agent_client_protocol::util::MatchDispatch;
use agent_client_protocol::{Agent, Client, ConnectTo, Dispatch, Handled, UntypedMessage};
use std::path::PathBuf;

/// Converts a `ContentBlock` to its string representation.
///
/// This function provides standard string conversions for different content types:
/// - `Text`: Returns the text content
/// - `Image`: Returns a placeholder like `[Image: image/png]`
/// - `Audio`: Returns a placeholder like `[Audio: audio/wav]`
/// - `ResourceLink`: Returns the URI
/// - `Resource`: Returns the URI
///
/// # Example
///
/// ```no_run
/// use yopo::content_block_to_string;
/// use agent_client_protocol::schema::{ContentBlock, TextContent};
///
/// let block = ContentBlock::Text(TextContent::new("Hello".to_string()));
/// assert_eq!(content_block_to_string(&block), "Hello");
/// ```
#[must_use]
pub fn content_block_to_string(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(TextContent { text, .. }) => text.clone(),
        ContentBlock::Image(ImageContent { mime_type, .. }) => {
            format!("[Image: {mime_type}]")
        }
        ContentBlock::Audio(AudioContent { mime_type, .. }) => {
            format!("[Audio: {mime_type}]")
        }
        ContentBlock::ResourceLink(link) => link.uri.clone(),
        ContentBlock::Resource(resource) => match &resource.resource {
            EmbeddedResourceResource::TextResourceContents(text) => text.uri.clone(),
            EmbeddedResourceResource::BlobResourceContents(blob) => blob.uri.clone(),
            _ => "[Unknown resource type]".to_string(),
        },
        _ => "[Unknown content type]".to_string(),
    }
}

/// Runs a single prompt against a component with a callback for each content block.
///
/// This function:
/// - Spawns the component
/// - Initializes the agent
/// - Creates a new session
/// - Sends the prompt
/// - Auto-approves all permission requests
/// - Calls the callback with each `ContentBlock` from agent messages
/// - Returns when the prompt completes
///
/// The callback receives each `ContentBlock` as it arrives and can process it
/// asynchronously (e.g., print it, accumulate it, etc.).
///
/// # Example
///
/// ```ignore
/// use yopo::{prompt_with_callback, content_block_to_string};
/// use agent_client_protocol_tokio::AcpAgent;
/// use std::str::FromStr;
///
/// # async fn example() -> Result<(), agent_client_protocol::Error> {
/// let agent = AcpAgent::from_str("python agent.py")?;
/// prompt_with_callback(agent, "What is 2+2?", async |block| {
///     print!("{}", content_block_to_string(&block));
/// }).await?;
/// # Ok(())
/// # }
/// ```
pub async fn prompt_with_callback(
    component: impl ConnectTo<Client>,
    prompt_text: impl ToString,
    mut callback: impl AsyncFnMut(ContentBlock) + Send,
) -> Result<(), agent_client_protocol::Error> {
    // Convert prompt to String
    let prompt_text = prompt_text.to_string();

    // Run the client
    Client
        .builder()
        .on_receive_dispatch(
            async |message: Dispatch<UntypedMessage, UntypedMessage>, _cx| {
                tracing::trace!("received: {:?}", message.message());
                Ok(Handled::No {
                    message,
                    retry: false,
                })
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_with(component, |cx: agent_client_protocol::ConnectionTo<Agent>| async move {
            // Initialize the agent
            let _init_response = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;

            let mut session = cx
                .build_session(PathBuf::from("."))
                .block_task()
                .start_session()
                .await?;

            session.send_prompt(prompt_text)?;

            loop {
                let update = session.read_update().await?;
                match update {
                    agent_client_protocol::SessionMessage::SessionMessage(message) => {
                        MatchDispatch::new(message)
                            .if_notification(async |notification: SessionNotification| {
                                tracing::debug!(
                                    ?notification,
                                    "yopo: received SessionNotification"
                                );
                                // Call the callback for each agent message chunk
                                if let agent_client_protocol::schema::SessionUpdate::AgentMessageChunk(
                                    content_chunk,
                                ) = notification.update
                                {
                                    callback(content_chunk.content).await;
                                }
                                Ok(())
                            })
                            .await
                            .if_request(async |request: RequestPermissionRequest, responder| {
                                // Auto-approve all permission requests by selecting the first option
                                // that looks "allow-ish"
                                let outcome = request
                                    .options
                                    .iter()
                                    .find(|option| match option.kind {
                                        agent_client_protocol::schema::PermissionOptionKind::AllowOnce
                                        | agent_client_protocol::schema::PermissionOptionKind::AllowAlways => true,
                                        agent_client_protocol::schema::PermissionOptionKind::RejectOnce
                                        | agent_client_protocol::schema::PermissionOptionKind::RejectAlways
                                        | _ => false,
                                    })
                                    .map_or(RequestPermissionOutcome::Cancelled, |option| {
                                        RequestPermissionOutcome::Selected(
                                            SelectedPermissionOutcome::new(
                                                option.option_id.clone(),
                                            ),
                                        )
                                    });

                                responder.respond(RequestPermissionResponse::new(outcome))?;

                                Ok(())
                            })
                            .await
                            .otherwise(async |_msg| Ok(()))
                            .await?;
                    }
                    agent_client_protocol::SessionMessage::StopReason(stop_reason) => {
                        match stop_reason {
                            agent_client_protocol::schema::StopReason::EndTurn => break,
                            agent_client_protocol::schema::StopReason::MaxTokens => {
                                tracing::debug!("Agent hit max tokens limit");
                                break;
                            }
                            agent_client_protocol::schema::StopReason::MaxTurnRequests => {
                                tracing::debug!("Agent hit max turn requests limit");
                                break;
                            }
                            agent_client_protocol::schema::StopReason::Refusal => {
                                tracing::warn!("Agent refused to continue");
                                break;
                            }
                            agent_client_protocol::schema::StopReason::Cancelled => {
                                tracing::debug!("Session was cancelled");
                                break;
                            }
                            other => {
                                tracing::warn!("Unknown stop reason: {:?}", other);
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }

            Ok(())
        })
        .await?;

    Ok(())
}

/// Runs a single prompt against a component and returns the accumulated text response.
///
/// This function:
/// - Spawns the component
/// - Initializes the agent
/// - Creates a new session
/// - Sends the prompt
/// - Auto-approves all permission requests
/// - Accumulates all content from agent messages using [`content_block_to_string`]
/// - Returns the complete response as a String
///
/// This is a convenience wrapper around [`prompt_with_callback`] that accumulates
/// all content blocks into a single string.
///
/// # Example
///
/// ```ignore
/// use yopo::prompt;
/// use agent_client_protocol_tokio::AcpAgent;
/// use std::str::FromStr;
///
/// # async fn example() -> Result<(), agent_client_protocol::Error> {
/// let agent = AcpAgent::from_str("python agent.py")?;
/// let response = prompt(agent, "What is 2+2?").await?;
/// assert!(response.contains("4"));
/// # Ok(())
/// # }
/// ```
pub async fn prompt(
    component: impl ConnectTo<Client>,
    prompt_text: impl ToString,
) -> Result<String, agent_client_protocol::Error> {
    let mut accumulated_text = String::new();
    prompt_with_callback(component, prompt_text, async |block| {
        let text = content_block_to_string(&block);
        accumulated_text.push_str(&text);
    })
    .await?;
    Ok(accumulated_text)
}
