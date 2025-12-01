//! Simple client implementation using the new `sacp` crate.
//!
//! This example shows how to connect to an agent and send requests using the handler chain API.

use agent_client_protocol::ProtocolVersion;
use sacp::JrHandlerChain;
use sacp::schema::{
    InitializeRequest, NewSessionRequest, PromptRequest, ReadTextFileRequest,
    RequestPermissionRequest, SessionNotification, WriteTextFileRequest,
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

// ANCHOR: setup
#[tokio::main]
async fn main() -> Result<(), sacp::Error> {
    let client_name = "sacp-client";

    JrHandlerChain::new()
        .name(client_name)
        // ANCHOR: handler_permission
        .on_receive_request(async move |_req: RequestPermissionRequest, cx| {
            eprintln!("[{}] Permission requested", client_name);
            cx.respond_with_error(sacp::Error::method_not_found())
        })
        // ANCHOR_END: handler_permission
        // ANCHOR: handler_file_ops
        .on_receive_request(async move |_req: WriteTextFileRequest, cx| {
            eprintln!("[{}] Writing file request", client_name);
            cx.respond_with_error(sacp::Error::method_not_found())
        })
        .on_receive_request(async move |_req: ReadTextFileRequest, cx| {
            eprintln!("[{}] Reading file request", client_name);
            cx.respond_with_error(sacp::Error::method_not_found())
        })
        // ANCHOR_END: handler_file_ops
        // ANCHOR: handler_notifications
        .on_receive_notification(async move |notif: SessionNotification, _cx| {
            eprintln!("[{}] Session update: {:?}", client_name, notif.update);
            Ok(())
        })
        // ANCHOR_END: handler_notifications
        // ANCHOR: with_client
        .with_client(
            sacp::ByteStreams::new(
                tokio::io::stdout().compat_write(),
                tokio::io::stdin().compat(),
            ),
            async |cx| {
                // ANCHOR: send_requests
                // Initialize the agent
                let init_response = cx
                    .send_request(InitializeRequest::new(ProtocolVersion::LATEST))
                    .block_task()
                    .await?;

                eprintln!("Agent initialized: {:?}", init_response.agent_info);

                // Create a session
                let session_response = cx
                    .send_request(NewSessionRequest::new("/"))
                    .block_task()
                    .await?;

                eprintln!("Session created: {}", session_response.session_id);

                // Send a prompt
                let prompt_response = cx
                    .send_request(PromptRequest::new(session_response.session_id, vec![]))
                    .block_task()
                    .await?;

                eprintln!("Prompt completed: {:?}", prompt_response.stop_reason);
                // ANCHOR_END: send_requests

                Ok(())
            },
        )
        .await
}
// ANCHOR_END: with_client
// ANCHOR_END: setup
