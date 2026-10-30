//! The protocol-v1 connection type and its conversions to and from the raw
//! connection.

use agent_client_protocol::schema::{ProtocolVersion, v1};
use agent_client_protocol::{Agent, Client, ConnectionTo, Error, RawJsonRpcId, V1};

#[tokio::test(flavor = "current_thread")]
async fn v1_sends_return_v1_typed_handles() -> Result<(), Error> {
    let agent = Agent.v1().on_receive_request(
        async |initialize: v1::InitializeRequest, responder, _cx| {
            responder.respond(v1::InitializeResponse::new(initialize.protocol_version))
        },
        agent_client_protocol::on_receive_request!(),
    );

    Client
        .v1()
        .connect_with(agent, async |cx| {
            let sent: agent_client_protocol::v1::SentRequest<v1::InitializeResponse> =
                cx.send_request(v1::InitializeRequest::new(ProtocolVersion::V1));
            let id: &v1::RequestId = sent.id();
            assert_eq!(RawJsonRpcId::from(id.clone()), *sent.raw_id());
            let response = sent.block_task().await?;
            assert_eq!(response.protocol_version, ProtocolVersion::V1);

            let prepared: agent_client_protocol::v1::PreparedRequest<v1::AuthenticateResponse> =
                cx.prepare_request(v1::AuthenticateRequest::new("none"));
            let _: &v1::RequestId = prepared.id();
            drop(prepared);
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn v1_and_raw_connections_convert_explicitly() -> Result<(), Error> {
    let agent = Agent.v1().on_receive_request(
        async |initialize: v1::InitializeRequest, responder, _cx| {
            responder.respond(v1::InitializeResponse::new(initialize.protocol_version))
        },
        agent_client_protocol::on_receive_request!(),
    );

    Client
        .v1()
        .connect_with(agent, async |cx| {
            let raw: ConnectionTo<Agent> = cx.clone().into_raw();
            let raw_sent = raw.send_request(v1::InitializeRequest::new(ProtocolVersion::V1));
            let _: &RawJsonRpcId = raw_sent.id();
            raw_sent.block_task().await?;

            let v1_cx: agent_client_protocol::v1::ConnectionTo<Agent> = raw.into_protocol::<V1>();
            let response = v1_cx
                .send_request(v1::InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            assert_eq!(response.protocol_version, ProtocolVersion::V1);
            Ok(())
        })
        .await
}
