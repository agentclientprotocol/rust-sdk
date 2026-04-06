use agent_client_protocol_core::schema::{
    AgentCapabilities, InitializeRequest, InitializeResponse,
};
use agent_client_protocol_core::{Agent, Client, ConnectionTo, Dispatch, Result};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[tokio::main]
async fn main() -> Result<()> {
    Agent
        .builder()
        .name("my-agent") // for debugging
        .on_receive_request(
            async move |initialize: InitializeRequest, responder, _connection| {
                // Respond to initialize successfully
                responder.respond(
                    InitializeResponse::new(initialize.protocol_version)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol_core::on_receive_request!(),
        )
        .on_receive_dispatch(
            async move |message: Dispatch, cx: ConnectionTo<Client>| {
                // Respond to any other message with an error
                message.respond_with_error(
                    agent_client_protocol_core::util::internal_error("TODO"),
                    cx,
                )
            },
            agent_client_protocol_core::on_receive_dispatch!(),
        )
        .connect_to(agent_client_protocol_core::ByteStreams::new(
            tokio::io::stdout().compat_write(),
            tokio::io::stdin().compat(),
        ))
        .await
}
