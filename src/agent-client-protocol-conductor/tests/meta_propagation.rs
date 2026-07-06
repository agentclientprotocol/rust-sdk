use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentBlock, InitializeRequest, InitializeResponse, Meta,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionId, StopReason,
    TextContent,
};
use agent_client_protocol::schema::{InitializeProxyRequest, ProtocolVersion};
use agent_client_protocol::util::MatchDispatchFrom;
use agent_client_protocol::{
    Agent, Client, Conductor, ConnectTo, ConnectionTo, Dispatch, HandleDispatchFrom, Handled,
    JsonRpcResponse, Proxy, SentRequest,
};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use serde_json::Value;
use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

async fn recv<T: JsonRpcResponse + Send>(
    response: SentRequest<T>,
) -> Result<T, agent_client_protocol::Error> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    response.on_receiving_result(async move |result| {
        tx.send(result)
            .map_err(|_| agent_client_protocol::Error::internal_error())
    })?;
    rx.await
        .map_err(|_| agent_client_protocol::Error::internal_error())?
}

fn trace_context_meta() -> Meta {
    let mut meta = Meta::new();
    meta.insert(
        "traceparent".into(),
        Value::String("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0b902b7-01".into()),
    );
    meta.insert(
        "tracestate".into(),
        Value::String("rojo=00f067aa0b902b7".into()),
    );
    meta.insert("baggage".into(), Value::String("tenant=acme".into()));
    meta
}

struct PassthroughProxy;

impl ConnectTo<Conductor> for PassthroughProxy {
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol::Error> {
        Proxy
            .builder()
            .name("passthrough-proxy")
            .on_receive_request_from(
                Client,
                async |request: InitializeProxyRequest, responder, cx| {
                    cx.send_request_to(Agent, request.initialize)
                        .forward_response_to(responder)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .with_handler(ForwardMessages)
            .connect_to(client)
            .await
    }
}

struct ForwardMessages;

impl HandleDispatchFrom<Conductor> for ForwardMessages {
    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        connection: ConnectionTo<Conductor>,
    ) -> Result<Handled<Dispatch>, agent_client_protocol::Error> {
        MatchDispatchFrom::new(message, &connection)
            .if_message_from(Client, async |message: Dispatch| {
                connection.send_proxied_message_to(Agent, message)?;
                Ok(Handled::Yes)
            })
            .await
            .if_message_from(Agent, async |message: Dispatch| {
                connection.send_proxied_message_to(Client, message)?;
                Ok(Handled::Yes)
            })
            .await
            .done()
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        "ForwardMessages"
    }
}

struct RecordingAgent {
    prompt_meta: Arc<Mutex<Option<Meta>>>,
}

impl ConnectTo<Client> for RecordingAgent {
    async fn connect_to(
        self,
        client: impl ConnectTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error> {
        let prompt_meta = self.prompt_meta;

        Agent
            .builder()
            .name("recording-agent")
            .on_receive_request(
                async |request: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(request.protocol_version)
                            .agent_capabilities(AgentCapabilities::new()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async |_request: NewSessionRequest, responder, _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("session-1")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: PromptRequest, responder, _cx| {
                    *prompt_meta.lock().expect("not poisoned") = request.meta;
                    responder.respond(PromptResponse::new(StopReason::EndTurn))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}

async fn run_with_conductor(
    components: ProxiesAndAgent,
    editor_task: impl AsyncFnOnce(ConnectionTo<Agent>) -> Result<(), agent_client_protocol::Error>,
) -> Result<(), agent_client_protocol::Error> {
    let (editor_out, conductor_in) = duplex(4096);
    let (conductor_out, editor_in) = duplex(4096);

    let transport =
        agent_client_protocol::ByteStreams::new(editor_out.compat_write(), editor_in.compat());

    Client
        .builder()
        .name("editor")
        .with_spawned(|_cx| async move {
            ConductorImpl::new_agent("conductor", components)
                .run(agent_client_protocol::ByteStreams::new(
                    conductor_out.compat_write(),
                    conductor_in.compat(),
                ))
                .await
        })
        .connect_with(transport, editor_task)
        .await
}

#[tokio::test]
async fn conductor_proxy_chain_preserves_prompt_meta() -> Result<(), agent_client_protocol::Error> {
    let observed_meta = Arc::new(Mutex::new(None));
    let expected_meta = trace_context_meta();
    let agent = RecordingAgent {
        prompt_meta: Arc::clone(&observed_meta),
    };

    run_with_conductor(
        ProxiesAndAgent::new(agent).proxy(PassthroughProxy),
        async |connection| {
            recv(connection.send_request(InitializeRequest::new(ProtocolVersion::V1))).await?;

            let session = recv(connection.send_request(NewSessionRequest::new("/"))).await?;
            recv(
                connection.send_request(
                    PromptRequest::new(
                        session.session_id,
                        vec![ContentBlock::Text(TextContent::new("hello"))],
                    )
                    .meta(expected_meta.clone()),
                ),
            )
            .await?;

            Ok(())
        },
    )
    .await?;

    assert_eq!(
        *observed_meta.lock().expect("not poisoned"),
        Some(expected_meta)
    );

    Ok(())
}
