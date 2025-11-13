//! A simple test proxy that adds `>` prefix to session update messages.
//!
//! This proxy demonstrates basic proxy functionality by intercepting
//! `session/update` notifications and prepending `>` to the content.

use sacp::schema::{ContentBlock, ContentChunk, SessionNotification, SessionUpdate};
use sacp::{Component, JrHandlerChain};
use sacp_proxy::{AcpProxyExt, McpServiceRegistry};

/// Run the arrow proxy that adds `>` to each session update.
///
/// # Arguments
///
/// * `transport` - Component to the predecessor (conductor or another proxy)
pub async fn run_arrow_proxy(transport: impl Component + 'static) -> Result<(), sacp::Error> {
    JrHandlerChain::new()
        .name("arrow-proxy")
        // Intercept session notifications from successor (agent) and modify them
        .on_receive_notification_from_successor(
            async |mut notification: SessionNotification, cx| {
                // Modify the content by adding > prefix
                match &mut notification.update {
                    SessionUpdate::AgentMessageChunk(ContentChunk { content, .. }) => {
                        // Add > prefix to text content
                        if let ContentBlock::Text(text_content) = content {
                            text_content.text = format!(">{}", text_content.text);
                        }
                    }
                    _ => {
                        // Don't modify other update types
                    }
                }

                // Forward modified notification to predecessor
                cx.send_notification(notification)?;
                Ok(())
            },
        )
        // Empty MCP registry - no tools provided
        .provide_mcp(McpServiceRegistry::default())
        // Enable proxy mode (handles capability handshake and message routing)
        .proxy()
        // Start serving
        .connect_to(transport)?
        .serve()
        .await
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_arrow_proxy_compiles() {
        // Basic smoke test that the arrow proxy module compiles
        // Full integration tests with conductor will be in sacp-conductor tests
    }
}
