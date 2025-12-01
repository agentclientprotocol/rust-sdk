//! Simple agent implementation using the new `sacp` crate.
//!
//! This example shows the handler-based approach for building agents.

use sacp::schema::{
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, ReadTextFileRequest, StopReason, WriteTextFileRequest,
};
use sacp::{JrHandlerChain, MessageAndCx, UntypedMessage};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

// ANCHOR: setup
#[tokio::main]
async fn main() -> Result<(), sacp::Error> {
    let agent_name = "sacp-agent";

    JrHandlerChain::new()
        .name(agent_name)
        // ANCHOR: handler_initialize
        .on_receive_request(async move |req: InitializeRequest, cx| {
            eprintln!("[{}] Initializing", agent_name);
            cx.respond(InitializeResponse::new(req.protocol_version))
        })
        // ANCHOR_END: handler_initialize
        // ANCHOR: handler_new_session
        .on_receive_request(async move |_req: NewSessionRequest, cx| {
            eprintln!("[{}] Creating session", agent_name);
            cx.respond(NewSessionResponse::new("session-1"))
        })
        // ANCHOR_END: handler_new_session
        // ANCHOR: handler_prompt
        .on_receive_request(async move |_req: PromptRequest, cx| {
            eprintln!("[{}] Processing prompt", agent_name);
            cx.respond(PromptResponse::new(StopReason::EndTurn))
        })
        // ANCHOR_END: handler_prompt
        // ANCHOR: handler_client_requests
        .on_receive_request(async move |_req: WriteTextFileRequest, cx| {
            eprintln!("[{}] Writing file request", agent_name);
            cx.respond_with_error(sacp::Error::method_not_found())
        })
        .on_receive_request(async move |_req: ReadTextFileRequest, cx| {
            eprintln!("[{}] Reading file request", agent_name);
            cx.respond_with_error(sacp::Error::method_not_found())
        })
        // ANCHOR_END: handler_client_requests
        // ANCHOR: handler_fallback
        .on_receive_message(
            async move |msg: MessageAndCx<UntypedMessage, UntypedMessage>| {
                // Fallback handler for any unhandled messages
                msg.respond_with_error(sacp::Error::method_not_found())
            },
        )
        // ANCHOR_END: handler_fallback
        .serve(sacp::ByteStreams::new(
            tokio::io::stdout().compat_write(),
            tokio::io::stdin().compat(),
        ))
        .await
}
// ANCHOR_END: setup
