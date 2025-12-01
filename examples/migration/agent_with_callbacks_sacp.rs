//! Agent that calls back to the client - showing the blocking risk.
//!
//! This example demonstrates how to handle requests that need to call back to the client,
//! and the importance of not blocking the message handler.

use sacp::schema::{
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, StopReason,
};
use sacp::{JrHandlerChain, MessageAndCx, UntypedMessage};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[tokio::main]
async fn main() -> Result<(), sacp::Error> {
    JrHandlerChain::new()
        .name("callback-agent")
        .on_receive_request(async move |req: InitializeRequest, cx| {
            cx.respond(InitializeResponse::new(req.protocol_version))
        })
        .on_receive_request(async move |_req: NewSessionRequest, cx| {
            cx.respond(NewSessionResponse::new("session-1"))
        })
        // ANCHOR: blocking_risk
        .on_receive_request(async move |_req: PromptRequest, cx| {
            // ❌ BAD: This will deadlock!
            // The handler is running on the message loop, so if we try to await
            // a request to the client, we'll block the loop from processing
            // the client's response.

            // This would deadlock:
            // let file_content = cx.connection_cx()
            //     .send_request(ReadTextFileRequest { ... })
            //     .block_task()
            //     .await?;
            // cx.respond(response)

            // ✅ GOOD: Respond immediately
            cx.respond(PromptResponse::new(StopReason::EndTurn))
        })
        // ANCHOR_END: blocking_risk
        // ANCHOR: spawn_alternative
        .on_receive_request(async move |_req: PromptRequest, cx| {
            // ALTERNATIVE: Use spawn for concurrent work
            let connection_cx = cx.connection_cx();

            let _ = connection_cx.spawn(async move {
                // This runs concurrently, not blocking the message loop
                // You can send requests to the client here
                eprintln!("Background task running");
                Ok(())
            });

            // Respond immediately
            cx.respond(PromptResponse::new(StopReason::EndTurn))
        })
        // ANCHOR_END: spawn_alternative
        .on_receive_message(
            async move |msg: MessageAndCx<UntypedMessage, UntypedMessage>| {
                msg.respond_with_error(sacp::Error::method_not_found())
            },
        )
        .serve(sacp::ByteStreams::new(
            tokio::io::stdout().compat_write(),
            tokio::io::stdin().compat(),
        ))
        .await
}
