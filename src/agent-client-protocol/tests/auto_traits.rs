//! Compile-time checks that the public handle types and the futures the SDK
//! returns are `Send`, `Sync` and `'static` where callers rely on it.
//!
//! Losing one of these auto traits is a breaking change that the type checker
//! would not otherwise report in this crate. Each check fails to compile if
//! the type stops implementing the trait. The `futures_*` functions are never
//! run. They exist only to be type-checked.

use std::future::Future;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema as v1_schema;
use agent_client_protocol::v1::schema::{
    InitializeRequest, InitializeResponse, NewSessionResponse, PromptRequest,
};
use agent_client_protocol::{
    Agent, Builder, Channel, Client, ConnectionDriver, ConnectionTo, Dispatch, DynConnectTo, Error,
    ErrorCode, PreparedRequest, Proxy, RawJsonRpcId, RawJsonRpcMessage, Responder, ResponseRouter,
    SentRequest, TransportFrame, UntypedMessage, UntypedRole, v1,
};

fn send<T: Send>() {}
fn sync<T: Sync>() {}
fn send_sync_static<T: Send + Sync + 'static>() {}
fn send_static<T: Send + 'static>() {}
fn send_future<F: Future + Send>(future: F) -> F {
    future
}

#[test]
fn connections_are_send_sync_static() {
    send_sync_static::<ConnectionTo<Agent>>();
    send_sync_static::<ConnectionTo<Client>>();
    send_sync_static::<v1::ConnectionTo<Agent>>();
    send_sync_static::<v1::ConnectionTo<Client>>();
    #[cfg(feature = "unstable_protocol_v2")]
    {
        send_sync_static::<agent_client_protocol::v2::ConnectionTo<Agent>>();
        send_sync_static::<agent_client_protocol::v2::ConnectionTo<Client>>();
    }
}

#[test]
fn builders_are_send_static() {
    send_static::<v1::Builder<Client>>();
    send_static::<v1::Builder<Agent>>();
    send_static::<v1::Builder<Proxy>>();
    send_static::<Builder<UntypedRole>>();
    #[cfg(feature = "unstable_protocol_v2")]
    send_static::<agent_client_protocol::v2::Builder<Client>>();
}

#[test]
fn request_handles_are_send_static() {
    send_static::<Responder<serde_json::Value>>();
    send_static::<v1::Responder<InitializeResponse>>();
    send_static::<ResponseRouter<serde_json::Value>>();
    send_static::<v1::ResponseRouter<InitializeResponse>>();
    send_static::<SentRequest<serde_json::Value>>();
    send_static::<v1::SentRequest<InitializeResponse>>();
    send_static::<PreparedRequest<serde_json::Value>>();
    send_static::<v1::PreparedRequest<InitializeResponse>>();
    send_static::<Dispatch>();
    send_static::<v1::Dispatch<PromptRequest>>();
    #[cfg(feature = "unstable_protocol_v2")]
    {
        use agent_client_protocol::v2;
        use agent_client_protocol::v2::schema as v2_schema;
        send_static::<v2::Responder<v2_schema::InitializeResponse>>();
        send_static::<v2::SentRequest<v2_schema::InitializeResponse>>();
        send_static::<v2::SentRequest<v2::InitializeOutcome>>();
        send_static::<v2::Dispatch>();
    }
}

#[test]
fn sessions_are_send() {
    send_static::<v1::ActiveSession<'static, Agent>>();
    send_static::<v1::SessionSetup<'static, Agent, NewSessionResponse>>();
    send_static::<v1::SessionBuilder<Agent>>();
    send_static::<v1::SessionMessage>();
    #[cfg(feature = "unstable_protocol_v2")]
    {
        use agent_client_protocol::v2;
        send_sync_static::<v2::ActiveSession<Agent>>();
        send_static::<v2::SessionBuilder<Agent>>();
        send_static::<v2::SessionSetup<Agent, agent_client_protocol::v2::schema::NewSessionResponse>>(
        );
    }
}

#[test]
fn transports_are_send_static() {
    send_static::<Channel>();
    send_static::<ConnectionDriver>();
    send_static::<DynConnectTo<Client>>();
    send_static::<DynConnectTo<Agent>>();
    send_sync_static::<TransportFrame>();
    send_sync_static::<RawJsonRpcMessage>();
    send_sync_static::<UntypedMessage>();
    #[cfg(all(feature = "process", not(target_family = "wasm")))]
    send_sync_static::<agent_client_protocol::AcpAgent>();
    #[cfg(all(feature = "stdio", not(target_family = "wasm")))]
    send_static::<agent_client_protocol::Stdio>();
}

#[test]
fn values_are_send_sync_static() {
    send_sync_static::<Error>();
    send_sync_static::<ErrorCode>();
    send_sync_static::<RawJsonRpcId>();
    send::<agent_client_protocol::Result<()>>();
    sync::<agent_client_protocol::Result<()>>();
}

/// Check that `f` returns a `Send` future, without running it.
fn send_future_fn<F, Fut>(_f: F)
where
    F: FnOnce(Channel) -> Fut,
    Fut: Future + Send,
{
}

#[test]
fn returned_futures_are_send() {
    // The compiler also checks the `send_future` calls inside each body.
    send_future_fn(futures_v1);
    send_future_fn(futures_agent);
    #[cfg(feature = "unstable_protocol_v2")]
    send_future_fn(futures_v2);
}

/// The futures returned by connecting, sending requests and starting a v1
/// session are `Send`, so they can run on a multi-threaded executor.
async fn futures_v1(transport: Channel) {
    let _connect = send_future(Client.v1().connect_with(
        transport,
        async |cx: v1::ConnectionTo<Agent>| {
            let _initialize = send_future(
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task(),
            );
            let _prepared = send_future(
                cx.prepare_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task(),
            );
            let builder = cx.build_session_cwd()?.block_task();
            let _start = send_future(builder.start_session());
            let _load = send_future(
                cx.load_session(v1_schema::SessionId::new("s"), "/")
                    .block_task()
                    .start_session(),
            );
            let _run_until = send_future(cx.build_session_cwd()?.block_task().run_until(
                async |v1::SessionSetup { mut session, .. }| {
                    send_future(session.read_update()).await?;
                    send_future(session.read_to_string()).await?;
                    Ok(())
                },
            ));
            Ok(())
        },
    ));
}

async fn futures_agent(transport: Channel) {
    let _serve = send_future(
        Agent
            .v1()
            .on_receive_request(
                async |request: InitializeRequest,
                       responder: v1::Responder<InitializeResponse>,
                       _cx: v1::ConnectionTo<Client>| {
                    responder.respond(InitializeResponse::new(request.protocol_version))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(transport),
    );
}

#[cfg(feature = "unstable_protocol_v2")]
async fn futures_v2(transport: Channel) {
    use agent_client_protocol::v2;
    use agent_client_protocol::v2::schema as v2_schema;

    let _connect = send_future(Client.v2().connect_with(
        transport,
        async |cx: v2::ConnectionTo<Agent>| {
            let request = v2_schema::InitializeRequest::new(
                ProtocolVersion::V2,
                v2_schema::Implementation::new("auto-traits", "0.0.0"),
            );
            let _initialize = send_future(cx.send_initialize(request).block_task());
            let _start = send_future(cx.build_session_cwd()?.start_session().block_task());
            Ok(())
        },
    ));
}
