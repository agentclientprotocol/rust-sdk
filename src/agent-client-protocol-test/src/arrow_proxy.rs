//! A simple test proxy that adds `>` prefix to session update messages.
//!
//! This proxy demonstrates basic proxy functionality by intercepting
//! `session/update` notifications and prepending `>` to the content.

use agent_client_protocol::schema::{
    ContentBlock, ContentChunk, SessionNotification, SessionUpdate,
};
use agent_client_protocol::{Agent, Client, ConnectTo, Proxy};

/// Run the arrow proxy that adds `>` to each session update.
///
/// # Arguments
///
/// * `transport` - Component to the predecessor (conductor or another proxy)
pub async fn run_arrow_proxy(
    transport: impl ConnectTo<Proxy> + 'static,
) -> Result<(), agent_client_protocol::Error> {
    Proxy
        .builder()
        .name("arrow-proxy")
        // Intercept session notifications from successor (agent) and modify them.
        // Using on_receive_notification_from(Agent, ...) automatically unwraps
        // SuccessorMessage envelopes.
        .on_receive_notification_from(
            Agent,
            async |mut notification: SessionNotification, cx| {
                // Modify the content by adding > prefix
                if let SessionUpdate::AgentMessageChunk(ContentChunk { content, .. }) =
                    &mut notification.update
                    // Add > prefix to text content
                    && let ContentBlock::Text(text_content) = content
                {
                    text_content.text = format!(">{}", text_content.text);
                } else {
                    // Don't modify other update types
                }

                // Forward modified notification to predecessor (client)
                cx.send_notification_to(Client, notification)?;
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(transport)
        .await
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_arrow_proxy_compiles() {
        // Basic smoke test that the arrow proxy module compiles
        // Full integration tests with conductor will be in agent-client-protocol-conductor tests
    }
}
