#![cfg(feature = "unstable_protocol_v2")]

use agent_client_protocol::schema::{
    AgentCapabilities, InitializeRequest, InitializeResponse, ProtocolVersion,
    WriteTextFileRequest, v2,
};
use agent_client_protocol::{
    Agent, ByteStreams, Client, ConnectionTo, JsonRpcResponse, Responder, SentRequest,
};
use futures::{AsyncRead, AsyncWrite};
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

fn setup_test_streams() -> (
    impl AsyncRead,
    impl AsyncWrite,
    impl AsyncRead,
    impl AsyncWrite,
) {
    let (client_writer, server_reader) = tokio::io::duplex(1024);
    let (server_writer, client_reader) = tokio::io::duplex(1024);

    (
        server_reader.compat(),
        server_writer.compat_write(),
        client_reader.compat(),
        client_writer.compat_write(),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn v1_client_can_initialize_v2_agent_handler() {
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async {
            let (server_reader, server_writer, client_reader, client_writer) = setup_test_streams();

            let agent = Agent.builder().on_receive_request(
                async |request: v2::InitializeRequest,
                       responder: Responder<v2::InitializeResponse>,
                       _connection: ConnectionTo<Client>| {
                    assert_eq!(request.protocol_version, ProtocolVersion::V1);
                    responder.respond(
                        v2::InitializeResponse::new(request.protocol_version)
                            .agent_capabilities(v2::AgentCapabilities::new()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            );

            let agent_transport = ByteStreams::new(server_writer, server_reader);
            tokio::task::spawn_local(async move {
                drop(agent.connect_to(agent_transport).await);
            });

            let client_transport = ByteStreams::new(client_writer, client_reader);
            let result = Client
                .connect_with(client_transport, async |connection| {
                    let response =
                        recv(connection.send_request(InitializeRequest::new(ProtocolVersion::V1)))
                            .await?;

                    assert_eq!(response.protocol_version, ProtocolVersion::V1);
                    Ok(())
                })
                .await;

            assert!(result.is_ok(), "initialize failed: {result:?}");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn v2_client_can_initialize_v1_agent_handler() {
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async {
            let (server_reader, server_writer, client_reader, client_writer) = setup_test_streams();

            let agent = Agent.builder().on_receive_request(
                async |request: InitializeRequest,
                       responder: Responder<InitializeResponse>,
                       _connection: ConnectionTo<Client>| {
                    assert_eq!(request.protocol_version, ProtocolVersion::V2);
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1)
                            .agent_capabilities(AgentCapabilities::new()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            );

            let agent_transport = ByteStreams::new(server_writer, server_reader);
            tokio::task::spawn_local(async move {
                drop(agent.connect_to(agent_transport).await);
            });

            let client_transport = ByteStreams::new(client_writer, client_reader);
            let result = Client
                .connect_with(client_transport, async |connection| {
                    let response = recv(
                        connection.send_request(v2::InitializeRequest::new(ProtocolVersion::V2)),
                    )
                    .await?;

                    assert_eq!(response.protocol_version, ProtocolVersion::V1);
                    Ok(())
                })
                .await;

            assert!(result.is_ok(), "initialize failed: {result:?}");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn v2_client_and_agent_track_negotiated_v2() {
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async {
            let (server_reader, server_writer, client_reader, client_writer) = setup_test_streams();

            let agent = Agent.builder().on_receive_request(
                async |request: v2::InitializeRequest,
                       responder: Responder<v2::InitializeResponse>,
                       connection: ConnectionTo<Client>| {
                    assert_eq!(request.protocol_version, ProtocolVersion::V2);
                    assert_eq!(connection.negotiated_protocol_version(), None);

                    responder.respond(
                        v2::InitializeResponse::new(ProtocolVersion::V2)
                            .agent_capabilities(v2::AgentCapabilities::new()),
                    )?;

                    assert_eq!(
                        connection.negotiated_protocol_version(),
                        Some(ProtocolVersion::V2)
                    );
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            );

            let agent_transport = ByteStreams::new(server_writer, server_reader);
            tokio::task::spawn_local(async move {
                drop(agent.connect_to(agent_transport).await);
            });

            let client_transport = ByteStreams::new(client_writer, client_reader);
            let result = Client
                .connect_with(client_transport, async |connection| {
                    assert_eq!(connection.negotiated_protocol_version(), None);

                    let response = recv(
                        connection.send_request(v2::InitializeRequest::new(ProtocolVersion::V2)),
                    )
                    .await?;

                    assert_eq!(response.protocol_version, ProtocolVersion::V2);
                    assert_eq!(
                        connection.negotiated_protocol_version(),
                        Some(ProtocolVersion::V2)
                    );
                    Ok(())
                })
                .await;

            assert!(result.is_ok(), "initialize failed: {result:?}");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn v2_client_handler_can_receive_v1_agent_request() {
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async {
            let (client_reader, client_writer, agent_reader, agent_writer) = setup_test_streams();

            let client = Client.builder().on_receive_request(
                async |request: v2::WriteTextFileRequest,
                       responder: Responder<v2::WriteTextFileResponse>,
                       _connection: ConnectionTo<Agent>| {
                    assert_eq!(request.session_id.0.as_ref(), "session:1");
                    assert_eq!(request.path, std::path::PathBuf::from("/tmp/acp-v2.txt"));
                    assert_eq!(request.content, "hello from v1");
                    responder.respond(v2::WriteTextFileResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            );

            let client_transport = ByteStreams::new(client_writer, client_reader);
            tokio::task::spawn_local(async move {
                drop(client.connect_to(client_transport).await);
            });

            let agent_transport = ByteStreams::new(agent_writer, agent_reader);
            let result = Agent
                .builder()
                .connect_with(agent_transport, async |connection| {
                    recv(connection.send_request(WriteTextFileRequest::new(
                        "session:1",
                        "/tmp/acp-v2.txt",
                        "hello from v1",
                    )))
                    .await?;

                    Ok(())
                })
                .await;

            assert!(result.is_ok(), "write_text_file failed: {result:?}");
        })
        .await;
}
