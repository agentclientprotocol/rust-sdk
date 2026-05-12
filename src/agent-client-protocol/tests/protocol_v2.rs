#![cfg(feature = "unstable_protocol_v2")]

use agent_client_protocol::schema::{
    AgentCapabilities, InitializeProxyRequest, InitializeRequest, InitializeResponse,
    ListSessionsRequest, ListSessionsResponse, NewSessionRequest, NewSessionResponse,
    ProtocolVersion, SessionId, SuccessorMessage,
};
use agent_client_protocol::{
    Agent, Channel, Client, Conductor, ConnectionTo, Handled, Proxy, Responder, UntypedMessage,
};
use std::sync::{Arc, Mutex};

async fn run_initialize_test(
    protocol_version: ProtocolVersion,
) -> agent_client_protocol::Result<()> {
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async move {
            assert!(
                std::any::type_name::<InitializeRequest>().contains("::v2::"),
                "unstable_protocol_v2 should make schema::* resolve to v2 types"
            );

            let (client_channel, agent_channel) = Channel::duplex();
            let expected_version = protocol_version;

            let agent = Agent
                .builder()
                .on_receive_request(
                    async move |initialize: InitializeRequest,
                                responder: Responder<InitializeResponse>,
                                cx: ConnectionTo<Client>| {
                        assert_eq!(cx.negotiated_protocol_version(), None);
                        responder.respond(
                            InitializeResponse::new(initialize.protocol_version)
                                .agent_capabilities(AgentCapabilities::new()),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |_request: NewSessionRequest,
                                responder: Responder<NewSessionResponse>,
                                cx: ConnectionTo<Client>| {
                        assert_eq!(cx.negotiated_protocol_version(), Some(expected_version));
                        responder.respond(NewSessionResponse::new(SessionId::new("session-1")))
                    },
                    agent_client_protocol::on_receive_request!(),
                );

            tokio::task::spawn_local(async move {
                agent.connect_to(agent_channel).await.ok();
            });

            Client
                .builder()
                .connect_with(client_channel, async move |cx| {
                    let initialize = cx
                        .send_request(InitializeRequest::new(protocol_version))
                        .block_task()
                        .await?;

                    assert_eq!(initialize.protocol_version, protocol_version);
                    assert_eq!(cx.negotiated_protocol_version(), Some(protocol_version));

                    let new_session = cx
                        .send_request(NewSessionRequest::new(
                            std::env::current_dir()
                                .map_err(agent_client_protocol::Error::into_internal_error)?,
                        ))
                        .block_task()
                        .await?;

                    assert_eq!(new_session.session_id, SessionId::new("session-1"));
                    Ok(())
                })
                .await
        })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn v2_schema_can_negotiate_v1_wire_version() -> agent_client_protocol::Result<()> {
    run_initialize_test(ProtocolVersion::V1).await
}

#[tokio::test(flavor = "current_thread")]
async fn v2_schema_can_negotiate_v2_wire_version() -> agent_client_protocol::Result<()> {
    run_initialize_test(ProtocolVersion::V2).await
}

#[tokio::test(flavor = "current_thread")]
async fn successor_request_responses_use_inner_method_for_conversion()
-> agent_client_protocol::Result<()> {
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async move {
            let (conductor_channel, proxy_channel) = Channel::duplex();

            let proxy = Proxy
                .builder()
                .on_receive_request_from(
                    Client,
                    async move |initialize: InitializeProxyRequest,
                                responder: Responder<InitializeResponse>,
                                _cx: ConnectionTo<Conductor>| {
                        responder.respond(
                            InitializeResponse::new(initialize.initialize.protocol_version)
                                .agent_capabilities(AgentCapabilities::new()),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request_from(
                    Agent,
                    async move |_request: NewSessionRequest,
                                responder: Responder<NewSessionResponse>,
                                _cx: ConnectionTo<Conductor>| {
                        let method = responder.method().to_string();
                        if method != "session/new" {
                            return responder.respond_with_error(
                                agent_client_protocol::Error::internal_error().data(method),
                            );
                        }

                        responder.respond(NewSessionResponse::new(SessionId::new("session-1")))
                    },
                    agent_client_protocol::on_receive_request!(),
                );

            tokio::task::spawn_local(async move {
                proxy.connect_to(proxy_channel).await.ok();
            });

            Conductor
                .builder()
                .connect_with(conductor_channel, async move |cx| {
                    cx.send_request(InitializeProxyRequest::from(InitializeRequest::new(
                        ProtocolVersion::V1,
                    )))
                    .block_task()
                    .await?;

                    let request = SuccessorMessage {
                        message: NewSessionRequest::new(
                            std::env::current_dir()
                                .map_err(agent_client_protocol::Error::into_internal_error)?,
                        ),
                        meta: None,
                    };

                    let response = cx.send_request(request);
                    assert_eq!(response.method(), "session/new");

                    let response = response.block_task().await?;
                    assert_eq!(response.session_id, SessionId::new("session-1"));
                    Ok(())
                })
                .await
        })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn successor_response_conversion_errors_use_inner_method() -> agent_client_protocol::Result<()>
{
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async move {
            let (conductor_channel, proxy_channel) = Channel::duplex();

            let proxy = Proxy
                .builder()
                .on_receive_request_from(
                    Client,
                    async move |initialize: InitializeProxyRequest,
                                responder: Responder<InitializeResponse>,
                                _cx: ConnectionTo<Conductor>| {
                        responder.respond(
                            InitializeResponse::new(initialize.initialize.protocol_version)
                                .agent_capabilities(AgentCapabilities::new()),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request_from(
                    Agent,
                    async move |request: UntypedMessage,
                                responder: Responder<serde_json::Value>,
                                _cx: ConnectionTo<Conductor>| {
                        if request.method() != "session/new" {
                            return Ok(Handled::No {
                                message: (request, responder),
                                retry: false,
                            });
                        }

                        responder.respond(serde_json::json!({"invalid": true}))?;
                        Ok(Handled::Yes)
                    },
                    agent_client_protocol::on_receive_request!(),
                );

            tokio::task::spawn_local(async move {
                proxy.connect_to(proxy_channel).await.ok();
            });

            Conductor
                .builder()
                .connect_with(conductor_channel, async move |cx| {
                    cx.send_request(InitializeProxyRequest::from(InitializeRequest::new(
                        ProtocolVersion::V1,
                    )))
                    .block_task()
                    .await?;

                    let request = SuccessorMessage {
                        message: NewSessionRequest::new(
                            std::env::current_dir()
                                .map_err(agent_client_protocol::Error::into_internal_error)?,
                        ),
                        meta: None,
                    };

                    let response = cx.send_request(request);
                    assert_eq!(response.method(), "session/new");

                    let error = response.block_task().await.unwrap_err();
                    assert_eq!(error.code, agent_client_protocol::ErrorCode::InvalidParams);
                    Ok(())
                })
                .await
        })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn initialize_request_sets_provisional_wire_version() -> agent_client_protocol::Result<()> {
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async move {
            let (client_channel, agent_channel) = Channel::duplex();
            let (error_tx, error_rx) = tokio::sync::oneshot::channel();
            let error_tx = Arc::new(Mutex::new(Some(error_tx)));

            let agent = Agent.builder().on_receive_request(
                {
                    let error_tx = error_tx.clone();
                    async move |initialize: InitializeRequest,
                                responder: Responder<InitializeResponse>,
                                cx: ConnectionTo<Client>| {
                        assert_eq!(cx.negotiated_protocol_version(), None);

                        let bad_request = UntypedMessage::new(
                            "session/new",
                            serde_json::json!({"invalid": true}),
                        )?;

                        if let Some(error_tx) = error_tx.lock().unwrap().take() {
                            cx.send_request(bad_request).on_receiving_result(
                                async move |result| {
                                    let error = result.unwrap_err();
                                    error_tx
                                        .send(error.code)
                                        .map_err(|_| agent_client_protocol::Error::internal_error())
                                },
                            )?;
                        }

                        responder.respond(
                            InitializeResponse::new(initialize.protocol_version)
                                .agent_capabilities(AgentCapabilities::new()),
                        )
                    }
                },
                agent_client_protocol::on_receive_request!(),
            );

            tokio::task::spawn_local(async move {
                agent.connect_to(agent_channel).await.ok();
            });

            Client
                .builder()
                .connect_with(client_channel, async move |cx| {
                    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;

                    assert_eq!(cx.negotiated_protocol_version(), Some(ProtocolVersion::V1));
                    let error_code = error_rx
                        .await
                        .map_err(agent_client_protocol::Error::into_internal_error)?;
                    assert_eq!(error_code, agent_client_protocol::ErrorCode::InvalidParams);
                    Ok(())
                })
                .await
        })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn outgoing_request_conversion_error_is_returned_to_sender()
-> agent_client_protocol::Result<()> {
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async move {
            let (client_channel, agent_channel) = Channel::duplex();

            let agent = Agent
                .builder()
                .on_receive_request(
                    async move |initialize: InitializeRequest,
                                responder: Responder<InitializeResponse>,
                                _cx: ConnectionTo<Client>| {
                        responder.respond(
                            InitializeResponse::new(initialize.protocol_version)
                                .agent_capabilities(AgentCapabilities::new()),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |_request: ListSessionsRequest,
                                responder: Responder<ListSessionsResponse>,
                                _cx: ConnectionTo<Client>| {
                        responder.respond(ListSessionsResponse::new(vec![]))
                    },
                    agent_client_protocol::on_receive_request!(),
                );

            tokio::task::spawn_local(async move {
                agent.connect_to(agent_channel).await.ok();
            });

            Client
                .builder()
                .connect_with(client_channel, async move |cx| {
                    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;

                    let bad_request =
                        UntypedMessage::new("session/new", serde_json::json!({"invalid": true}))?;
                    let error = cx.send_request(bad_request).block_task().await.unwrap_err();
                    assert_eq!(error.code, agent_client_protocol::ErrorCode::InvalidParams);

                    let sessions = cx
                        .send_request(ListSessionsRequest::new())
                        .block_task()
                        .await?;
                    assert!(sessions.sessions.is_empty());
                    Ok(())
                })
                .await
        })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn outgoing_response_conversion_error_is_sent_as_json_rpc_error()
-> agent_client_protocol::Result<()> {
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async move {
            let (client_channel, agent_channel) = Channel::duplex();

            let agent = Agent
                .builder()
                .on_receive_request(
                    async move |initialize: InitializeRequest,
                                responder: Responder<InitializeResponse>,
                                _cx: ConnectionTo<Client>| {
                        responder.respond(
                            InitializeResponse::new(initialize.protocol_version)
                                .agent_capabilities(AgentCapabilities::new()),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |_request: ListSessionsRequest,
                                responder: Responder<ListSessionsResponse>,
                                _cx: ConnectionTo<Client>| {
                        responder.respond(ListSessionsResponse::new(vec![]))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: UntypedMessage,
                                responder: Responder<serde_json::Value>,
                                _cx: ConnectionTo<Client>| {
                        if request.method() != "session/new" {
                            return Ok(Handled::No {
                                message: (request, responder),
                                retry: false,
                            });
                        }

                        responder.respond(serde_json::json!({"invalid": true}))?;
                        Ok(Handled::Yes)
                    },
                    agent_client_protocol::on_receive_request!(),
                );

            tokio::task::spawn_local(async move {
                agent.connect_to(agent_channel).await.ok();
            });

            Client
                .builder()
                .connect_with(client_channel, async move |cx| {
                    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;

                    let error = cx
                        .send_request(NewSessionRequest::new(
                            std::env::current_dir()
                                .map_err(agent_client_protocol::Error::into_internal_error)?,
                        ))
                        .block_task()
                        .await
                        .unwrap_err();
                    assert_eq!(error.code, agent_client_protocol::ErrorCode::InvalidParams);

                    let sessions = cx
                        .send_request(ListSessionsRequest::new())
                        .block_task()
                        .await?;
                    assert!(sessions.sessions.is_empty());
                    Ok(())
                })
                .await
        })
        .await
}
