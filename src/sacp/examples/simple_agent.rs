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
                agent_capabilities: AgentCapabilities {
                    load_session: Default::default(),
                    prompt_capabilities: Default::default(),
                    mcp_capabilities: Default::default(),
                    meta: Default::default(),
                },
                auth_methods: Default::default(),
                agent_info: Default::default(),
                meta: Default::default(),
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
