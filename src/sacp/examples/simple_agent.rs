use sacp::schema::{AgentCapabilities, InitializeRequest, InitializeResponse};
use sacp::{JrHandlerChain, MessageAndCx, UntypedMessage};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[tokio::main]
async fn main() -> Result<(), sacp::Error> {
    JrHandlerChain::new()
        .name("my-agent") // for debugging
        .on_receive_request(async move |initialize: InitializeRequest, request_cx| {
            // Respond to initialize successfully
            request_cx.respond(InitializeResponse {
                protocol_version: initialize.protocol_version,
                agent_capabilities: AgentCapabilities::default(),
                auth_methods: vec![],
                agent_info: None,
                meta: None,
            })
        })
        .on_receive_message(
            async move |message: MessageAndCx<UntypedMessage, UntypedMessage>| {
                // Respond to any other message with an error
                message.respond_with_error(sacp::util::internal_error("TODO"))
            },
        )
        .serve(sacp::ByteStreams::new(
            tokio::io::stdout().compat_write(),
            tokio::io::stdin().compat(),
        ))
        .await
}
