#![cfg(feature = "unstable_mcp_over_acp")]

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use agent_client_protocol::{
    Agent, Channel, Client, Error, Responder,
    mcp_client::McpOverAcp,
    role,
    schema::v1::{
        ConnectMcpRequest, ConnectMcpResponse, DisconnectMcpRequest, DisconnectMcpResponse,
        McpConnectionId, McpServerAcpId, MessageMcpNotification, MessageMcpRequest,
        MessageMcpResponse,
    },
};
use futures::channel::oneshot;
use serde_json::{Value, json};

#[tokio::test]
async fn native_mcp_initialize_tools_callback_and_close() {
    let closed = Arc::new(AtomicBool::new(false));
    let closed_provider = closed.clone();
    let (reverse_result_tx, reverse_result_rx) = oneshot::channel();
    let reverse_result_tx = Arc::new(Mutex::new(Some(reverse_result_tx)));
    let (reverse_received_tx, reverse_received_rx) = oneshot::channel();
    let reverse_received_tx = Mutex::new(Some(reverse_received_tx));
    let held_reverse_responder = Arc::new(Mutex::new(None::<Responder<Value>>));
    let held_for_handler = held_reverse_responder.clone();
    let (channel_agent, channel_client) = Channel::duplex();
    let provider = Client.builder()
        .on_receive_request(
            async |request: ConnectMcpRequest, responder: Responder<ConnectMcpResponse>, cx| {
                assert_eq!(request.server_id.0.as_ref(), "demo-server");
                responder.respond(ConnectMcpResponse::new(McpConnectionId::new("demo-connection")))?;
                cx.send_notification(MessageMcpNotification::new(
                    McpConnectionId::new("demo-connection"), "notifications/tools/list_changed",
                ))?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: MessageMcpRequest, responder: Responder<MessageMcpResponse>, cx| {
                assert_eq!(request.connection_id.0.as_ref(), "demo-connection");
                let result = match request.method.as_str() {
                    "initialize" => {
                        assert!(request.params.is_some());
                        json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"demo","version":"1"}})
                    }
                    "tools/list" => {
                        assert!(request.params.is_none());
                        let reverse_result_tx = reverse_result_tx.clone();
                        cx.send_request(MessageMcpRequest::new(
                            McpConnectionId::new("demo-connection"), "sampling/createMessage",
                        )).on_receiving_result(move |result| {
                            if let Some(tx) = reverse_result_tx.lock().unwrap().take() {
                                let _ = tx.send(result.is_err());
                            }
                            futures::future::ready(Ok(()))
                        })?;
                        json!({"tools":[{"name":"echo","description":"Echo","inputSchema":{"type":"object"}}]})
                    }
                    other => panic!("unexpected MCP request {other}"),
                };
                responder.respond(agent_client_protocol::JsonRpcResponse::from_value("mcp/message", result)?)?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: DisconnectMcpRequest, responder: Responder<DisconnectMcpResponse>, _| {
                assert_eq!(request.connection_id.0.as_ref(), "demo-connection");
                closed_provider.store(true, Ordering::Release);
                responder.respond(DisconnectMcpResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        );
    let agent = Agent.builder().connect_with(channel_agent, async |cx| {
        let (transport, close) =
            McpOverAcp::connect_v1(&cx, McpServerAcpId::new("demo-server")).await?;
        let (notice_tx, notice_rx) = oneshot::channel();
        let notice_tx = Mutex::new(Some(notice_tx));
        let mcp_client = role::mcp::Client
            .builder()
            .on_receive_notification(
                async move |notice: agent_client_protocol::UntypedMessage, _| {
                    assert_eq!(notice.method(), "notifications/tools/list_changed");
                    if let Some(tx) = notice_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: agent_client_protocol::UntypedMessage,
                            responder: Responder<Value>,
                            _| {
                    assert_eq!(request.method(), "sampling/createMessage");
                    *held_for_handler.lock().unwrap() = Some(responder);
                    if let Some(tx) = reverse_received_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            );
        mcp_client
            .connect_with(transport, async |mcp| {
                let init = mcp
                    .send_request(agent_client_protocol::UntypedMessage {
                        method: "initialize".into(),
                        params: json!({
                            "protocolVersion":"2024-11-05",
                            "capabilities":{},
                            "clientInfo":{"name":"consumer","version":"1"}
                        }),
                    })
                    .block_task()
                    .await?;
                assert_eq!(init["serverInfo"]["name"], "demo");
                let listed = mcp
                    .send_request(agent_client_protocol::UntypedMessage {
                        method: "tools/list".into(),
                        params: Value::Null,
                    })
                    .block_task()
                    .await?;
                assert_eq!(listed["tools"][0]["name"], "echo");
                notice_rx.await.map_err(Error::into_internal_error)?;
                reverse_received_rx
                    .await
                    .map_err(Error::into_internal_error)?;
                close.close().await?;
                assert!(
                    reverse_result_rx
                        .await
                        .map_err(Error::into_internal_error)?,
                    "pending reverse request should fail when MCP transport closes"
                );
                Ok(())
            })
            .await?;
        Ok(())
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        futures::try_join!(provider.connect_to(channel_client), agent)
    })
    .await
    .expect("native MCP connection timed out")
    .expect("native MCP connection failed");
    assert!(
        closed.load(Ordering::Acquire),
        "provider did not receive disconnect"
    );
    drop(held_reverse_responder.lock().unwrap().take());
}

#[cfg(feature = "unstable_protocol_v2")]
#[tokio::test]
async fn draft_v2_uses_the_same_native_mcp_transport() {
    use agent_client_protocol::{
        mcp_client::V2,
        schema::{ProtocolVersion, v2},
    };

    let (agent_channel, client_channel) = Channel::duplex();
    let (initialized_tx, initialized_rx) = oneshot::channel();
    let provider = Client
        .v2()
        .on_receive_request(
            async |request: v2::ConnectMcpRequest,
                   responder: Responder<v2::ConnectMcpResponse>,
                   _| {
                assert_eq!(request.server_id.0.as_ref(), "v2-server");
                responder.respond(v2::ConnectMcpResponse::new("v2-connection"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: v2::MessageMcpRequest,
                   responder: Responder<v2::MessageMcpResponse>,
                   _| {
                assert_eq!(request.connection_id.0.as_ref(), "v2-connection");
                assert_eq!(request.method, "tools/list");
                assert!(request.params.is_none());
                responder.respond(agent_client_protocol::JsonRpcResponse::from_value(
                    "mcp/message",
                    json!({"tools":[{"name":"v2-echo","inputSchema":{"type":"object"}}]}),
                )?)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: v2::DisconnectMcpRequest,
                   responder: Responder<v2::DisconnectMcpResponse>,
                   _| {
                assert_eq!(request.connection_id.0.as_ref(), "v2-connection");
                responder.respond(v2::DisconnectMcpResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        );
    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _| {
                responder.respond(
                    v2::InitializeResponse::new(
                        request.protocol_version,
                        v2::Implementation::new("consumer-test-agent", "1"),
                    )
                    .capabilities(
                        v2::AgentCapabilities::new()
                            .session(v2::SessionCapabilities::new().mcp(
                                v2::McpCapabilities::new().acp(v2::McpAcpCapabilities::new()),
                            )),
                    ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent_channel, async |cx| {
            initialized_rx.await.map_err(Error::into_internal_error)?;
            let (transport, close) =
                McpOverAcp::<V2>::connect_v2(&cx, v2::McpServerAcpId::new("v2-server")).await?;
            role::mcp::Client
                .builder()
                .connect_with(transport, async |mcp| {
                    let result = mcp
                        .send_request(agent_client_protocol::UntypedMessage {
                            method: "tools/list".into(),
                            params: Value::Null,
                        })
                        .block_task()
                        .await?;
                    assert_eq!(result["tools"][0]["name"], "v2-echo");
                    close.close().await
                })
                .await
        });
    let provider = provider.connect_with(client_channel, async |cx| {
        cx.send_request(v2::InitializeRequest::new(
            ProtocolVersion::V2,
            v2::Implementation::new("consumer-test-client", "1"),
        ))
        .block_task()
        .await?;
        let _ = initialized_tx.send(());
        cx.incoming_closed().await;
        Ok(())
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        futures::try_join!(provider, agent)
    })
    .await
    .expect("v2 MCP connection timed out")
    .expect("v2 MCP connection failed");
}

#[tokio::test]
async fn cancelled_connect_disconnects_if_provider_opens_later() {
    let (held_tx, held_rx) = oneshot::channel();
    let held_tx = Mutex::new(Some(held_tx));
    let (disconnected_tx, disconnected_rx) = oneshot::channel();
    let disconnected_tx = Mutex::new(Some(disconnected_tx));
    let (agent_channel, client_channel) = Channel::duplex();
    let provider = Client
        .builder()
        .on_receive_request(
            async move |_: ConnectMcpRequest, responder: Responder<ConnectMcpResponse>, _| {
                if let Some(tx) = held_tx.lock().unwrap().take() {
                    let _ = tx.send(responder);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: DisconnectMcpRequest,
                        responder: Responder<DisconnectMcpResponse>,
                        _| {
                assert_eq!(request.connection_id.0.as_ref(), "late-connection");
                if let Some(tx) = disconnected_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                responder.respond(DisconnectMcpResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        );
    let agent = Agent.builder().connect_with(agent_channel, async |cx| {
        let mut pending = Box::pin(McpOverAcp::connect_v1(
            &cx,
            McpServerAcpId::new("late-server"),
        ));
        let responder = tokio::select! {
            result = &mut pending => panic!("connect completed before provider responded: {result:?}"),
            result = held_rx => result.map_err(Error::into_internal_error)?,
        };
        drop(pending);
        responder.respond(ConnectMcpResponse::new(McpConnectionId::new(
            "late-connection",
        )))?;
        disconnected_rx.await.map_err(Error::into_internal_error)?;
        Ok(())
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        futures::try_join!(provider.connect_to(client_channel), agent)
    })
    .await
    .expect("cancelled connect cleanup timed out")
    .expect("cancelled connect cleanup failed");
}
