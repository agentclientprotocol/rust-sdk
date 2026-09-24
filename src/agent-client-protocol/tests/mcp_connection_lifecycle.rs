#![cfg(all(feature = "unstable_mcp_over_acp", feature = "unstable_protocol_v2"))]

use std::{future::pending, time::Duration};

use agent_client_protocol::{
    Agent, Client, ConnectTo, ConnectionTo, DynConnectTo, Error, JsonRpcRequest, JsonRpcResponse,
    NullRun, Responder, UntypedMessage, V2ConnectionTo,
    mcp_server::{McpConnectionTo, McpServer, McpServerConnect},
    role,
    schema::{ProtocolVersion, v1, v2},
};
use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_test/echo", response = EchoResponse)]
struct EchoRequest {
    value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
struct EchoResponse {
    value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_test/pending", response = EchoResponse)]
struct PendingRequest {}

struct Probe {
    id: String,
    dropped: mpsc::UnboundedSender<String>,
}

impl Drop for Probe {
    fn drop(&mut self) {
        drop(self.dropped.unbounded_send(self.id.clone()));
    }
}

struct Server {
    dropped: mpsc::UnboundedSender<String>,
    failures: mpsc::UnboundedSender<(String, oneshot::Sender<()>)>,
    pending_started: mpsc::UnboundedSender<()>,
    reverse_seen: mpsc::UnboundedSender<String>,
}

impl McpServerConnect<Agent> for Server {
    fn name(&self) -> String {
        "lifecycle-test".into()
    }

    fn connect(&self, context: McpConnectionTo<Agent>) -> DynConnectTo<role::mcp::Client> {
        let id = context.connection_id().unwrap().to_string();
        let (fail, failure) = oneshot::channel();
        self.failures.unbounded_send((id.clone(), fail)).unwrap();
        DynConnectTo::new(ServerConnection {
            probe: Probe {
                id,
                dropped: self.dropped.clone(),
            },
            failure,
            pending_started: self.pending_started.clone(),
            reverse_seen: self.reverse_seen.clone(),
        })
    }
}

struct ServerConnection {
    probe: Probe,
    failure: oneshot::Receiver<()>,
    pending_started: mpsc::UnboundedSender<()>,
    reverse_seen: mpsc::UnboundedSender<String>,
}

impl ConnectTo<role::mcp::Client> for ServerConnection {
    async fn connect_to(self, client: impl ConnectTo<role::mcp::Server>) -> Result<(), Error> {
        let pending_started = self.pending_started;
        role::mcp::Server
            .builder()
            .on_receive_request(
                async |request: EchoRequest, responder: Responder<EchoResponse>, _connection| {
                    responder.respond(EchoResponse {
                        value: request.value,
                    })
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_request: PendingRequest,
                            _responder: Responder<EchoResponse>,
                            _connection| {
                    pending_started
                        .unbounded_send(())
                        .map_err(Error::into_internal_error)?;
                    pending::<()>().await;
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .with_spawned(move |connection| async move {
                let _probe = self.probe;
                connection.send_notification(UntypedMessage {
                    method: "_test/reverse-notice".into(),
                    params: Value::Null,
                })?;
                let response = connection
                    .send_request(EchoRequest {
                        value: "reverse".into(),
                    })
                    .block_task()
                    .await?;
                assert_eq!(response.value, "reverse");
                assert!(
                    connection
                        .send_request(EchoRequest {
                            value: "error".into()
                        })
                        .block_task()
                        .await
                        .is_err()
                );
                self.reverse_seen
                    .unbounded_send(_probe.id.clone())
                    .map_err(Error::into_internal_error)?;
                self.failure.await.map_err(Error::into_internal_error)?;
                Err(Error::internal_error().data("child failure"))
            })
            .connect_to(client)
            .await
    }
}

fn implementation() -> v2::Implementation {
    v2::Implementation::new("lifecycle-test", env!("CARGO_PKG_VERSION"))
}

async fn echo(
    connection: &V2ConnectionTo<Client>,
    id: v2::McpConnectionId,
    value: &str,
) -> Result<(), Error> {
    let response = connection
        .send_request(
            v2::MessageMcpRequest::new(id, "_test/echo").params(
                serde_json::from_value::<serde_json::Map<String, Value>>(json!({"value": value}))
                    .unwrap(),
            ),
        )
        .block_task()
        .await?;
    let response: Value =
        serde_json::from_str(response.0.get()).map_err(Error::into_internal_error)?;
    assert_eq!(response, json!({"value": value}));
    Ok(())
}

async fn scenario(
    connection: V2ConnectionTo<Client>,
    server: v2::McpServerAcpId,
    mut dropped: mpsc::UnboundedReceiver<String>,
    mut failures: mpsc::UnboundedReceiver<(String, oneshot::Sender<()>)>,
    mut pending_started: mpsc::UnboundedReceiver<()>,
    mut reverse_seen: mpsc::UnboundedReceiver<String>,
    mut reverse_notices: mpsc::UnboundedReceiver<String>,
) -> Result<(), Error> {
    let a = connection
        .send_request(v2::ConnectMcpRequest::new(server.clone()))
        .block_task()
        .await?
        .connection_id;
    let b = connection
        .send_request(v2::ConnectMcpRequest::new(server))
        .block_task()
        .await?
        .connection_id;
    assert_ne!(a, b);
    let (id_a, _fail_a) = failures.next().await.unwrap();
    let (id_b, fail_b) = failures.next().await.unwrap();
    assert_eq!((id_a, id_b), (a.to_string(), b.to_string()));
    assert_eq!(reverse_seen.next().await, Some(a.to_string()));
    assert_eq!(reverse_seen.next().await, Some(b.to_string()));
    assert_eq!(reverse_notices.next().await, Some(a.to_string()));
    assert_eq!(reverse_notices.next().await, Some(b.to_string()));
    echo(&connection, a.clone(), "A").await?;
    echo(&connection, b.clone(), "B").await?;

    // Leave a request outstanding when A disconnects. Neither that request
    // nor the other half of A may keep its server alive.
    let (pending_result_tx, pending_result_rx) = oneshot::channel();
    let pending_connection = connection.clone();
    let pending_id = a.clone();
    connection.spawn(async move {
        let result = pending_connection
            .send_request(
                v2::MessageMcpRequest::new(pending_id, "_test/pending")
                    .params(serde_json::Map::new()),
            )
            .block_task()
            .await;
        drop(pending_result_tx.send(result));
        Ok(())
    })?;
    pending_started
        .next()
        .await
        .expect("pending request reached server");
    connection
        .send_request(v2::DisconnectMcpRequest::new(a.clone()))
        .block_task()
        .await?;
    assert_eq!(dropped.next().await, Some(a.to_string()));
    assert!(
        pending_result_rx.await.unwrap().is_err(),
        "pending request must fail on disconnect"
    );
    assert!(
        connection
            .send_request(v2::DisconnectMcpRequest::new(a.clone()))
            .block_task()
            .await
            .is_err()
    );
    assert!(
        connection
            .send_request(v2::DisconnectMcpRequest::new(v2::McpConnectionId::new(
                "missing"
            )))
            .block_task()
            .await
            .is_err()
    );
    assert!(
        connection
            .send_request(v2::MessageMcpRequest::new(a, "_test/echo"))
            .block_task()
            .await
            .is_err()
    );
    echo(&connection, b.clone(), "still alive").await?;
    fail_b.send(()).unwrap();
    assert_eq!(dropped.next().await, Some(b.to_string()));
    assert!(
        connection
            .send_request(v2::DisconnectMcpRequest::new(b))
            .block_task()
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_connections_close_independently_without_closing_acp() -> Result<(), Error> {
    let test = async {
        let (server_tx, mut server_rx) = mpsc::unbounded();
        let (dropped_tx, dropped_rx) = mpsc::unbounded();
        let (failure_tx, failure_rx) = mpsc::unbounded();
        let (pending_started_tx, pending_started_rx) = mpsc::unbounded();
        let (reverse_seen_tx, reverse_seen_rx) = mpsc::unbounded();
        let (reverse_notice_tx, reverse_notice_rx) = mpsc::unbounded();
        let (result_tx, mut result_rx) = mpsc::unbounded();
        let agent = Agent
            .v2()
            .on_receive_request(
                async |request: v2::InitializeRequest,
                       responder: Responder<v2::InitializeResponse>,
                       _connection: V2ConnectionTo<Client>| {
                    responder.respond(
                        v2::InitializeResponse::new(request.protocol_version, implementation())
                            .capabilities(v2::AgentCapabilities::new().session(
                                v2::SessionCapabilities::new().mcp(
                                    v2::McpCapabilities::new().acp(v2::McpAcpCapabilities::new()),
                                ),
                            )),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: v2::NewSessionRequest,
                            responder: Responder<v2::NewSessionResponse>,
                            _connection: V2ConnectionTo<Client>| {
                    let [v2::McpServer::Acp(server)] = request.mcp_servers.as_slice() else {
                        panic!("missing native server")
                    };
                    server_tx
                        .unbounded_send(server.server_id.clone())
                        .map_err(Error::into_internal_error)?;
                    responder.respond(v2::NewSessionResponse::new(v2::SessionId::new("lifecycle")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async |request: v2::MessageMcpRequest,
                       responder: Responder<v2::MessageMcpResponse>,
                       _connection: V2ConnectionTo<Client>| {
                    assert_eq!(request.method, "_test/echo");
                    let value = request
                        .params
                        .as_ref()
                        .and_then(|params| params.get("value"));
                    if value == Some(&json!("error")) {
                        return responder.respond_with_error(Error::invalid_params());
                    }
                    assert_eq!(value, Some(&json!("reverse")));
                    let raw = serde_json::value::to_raw_value(&json!({"value": "reverse"}))
                        .map_err(Error::into_internal_error)?;
                    responder.respond(v2::MessageMcpResponse::new(raw.into()))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                async move |notification: v2::MessageMcpNotification,
                            _connection: V2ConnectionTo<Client>| {
                    assert_eq!(notification.method, "_test/reverse-notice");
                    assert!(
                        notification.params.is_none(),
                        "null params must stay omitted"
                    );
                    reverse_notice_tx
                        .unbounded_send(notification.connection_id.to_string())
                        .map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .with_spawned(move |connection: V2ConnectionTo<Client>| async move {
                let server = server_rx.next().await.unwrap();
                let result = scenario(
                    connection,
                    server,
                    dropped_rx,
                    failure_rx,
                    pending_started_rx,
                    reverse_seen_rx,
                    reverse_notice_rx,
                )
                .await;
                result_tx
                    .unbounded_send(result)
                    .map_err(Error::into_internal_error)
            });

        Client
            .v2()
            .connect_with(agent, async move |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        implementation(),
                    ))
                    .block_task()
                    .await?;
                let session = connection
                    .build_session_from(v2::NewSessionRequest::new(
                        std::env::current_dir().map_err(Error::into_internal_error)?,
                    ))
                    .with_mcp_server(McpServer::<Agent, _>::new(
                        Server {
                            dropped: dropped_tx,
                            failures: failure_tx,
                            pending_started: pending_started_tx,
                            reverse_seen: reverse_seen_tx,
                        },
                        NullRun,
                    ))?
                    .start_session()
                    .block_task()
                    .await?;
                let result = result_rx
                    .next()
                    .await
                    .expect("agent scenario did not complete");
                drop(session);
                result
            })
            .await
    };
    tokio::time::timeout(Duration::from_secs(10), test)
        .await
        .expect("native lifecycle timed out")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v1_native_connections_share_the_same_isolated_lifecycle() -> Result<(), Error> {
    let test = async {
        let (server_tx, mut server_rx) = mpsc::unbounded();
        let (dropped_tx, mut dropped_rx) = mpsc::unbounded();
        let (failure_tx, mut failure_rx) = mpsc::unbounded::<(String, oneshot::Sender<()>)>();
        let (reverse_seen_tx, mut reverse_seen_rx) = mpsc::unbounded();
        let (reverse_notice_tx, mut reverse_notice_rx) = mpsc::unbounded();
        let (result_tx, mut result_rx) = mpsc::unbounded();
        let (pending_started_tx, _pending_started_rx) = mpsc::unbounded();
        let agent = Agent
            .builder()
            .on_receive_request(
                async move |request: v1::NewSessionRequest,
                            responder: Responder<v1::NewSessionResponse>,
                            _connection: ConnectionTo<Client>| {
                    let [v1::McpServer::Acp(server)] = request.mcp_servers.as_slice() else {
                        panic!("missing v1 native server")
                    };
                    server_tx
                        .unbounded_send(server.server_id.clone())
                        .map_err(Error::into_internal_error)?;
                    responder.respond(v1::NewSessionResponse::new("v1-lifecycle"))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async |request: v1::MessageMcpRequest,
                       responder: Responder<v1::MessageMcpResponse>,
                       _connection: ConnectionTo<Client>| {
                    assert_eq!(request.method, "_test/echo");
                    if request
                        .params
                        .as_ref()
                        .and_then(|params| params.get("value"))
                        == Some(&json!("error"))
                    {
                        return responder.respond_with_error(Error::invalid_params());
                    }
                    let raw = serde_json::value::to_raw_value(&json!({"value": "reverse"}))
                        .map_err(Error::into_internal_error)?;
                    responder.respond(v1::MessageMcpResponse::new(raw.into()))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                async move |notification: v1::MessageMcpNotification,
                            _connection: ConnectionTo<Client>| {
                    assert_eq!(notification.method, "_test/reverse-notice");
                    assert!(notification.params.is_none());
                    reverse_notice_tx
                        .unbounded_send(notification.connection_id.to_string())
                        .map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .with_spawned(move |connection: ConnectionTo<Client>| async move {
                let server = server_rx.next().await.unwrap();
                let result = async {
                    let a = connection
                        .send_request(v1::ConnectMcpRequest::new(server.clone()))
                        .block_task()
                        .await?
                        .connection_id;
                    let b = connection
                        .send_request(v1::ConnectMcpRequest::new(server))
                        .block_task()
                        .await?
                        .connection_id;
                    assert_ne!(a, b);
                    let (id_a, _failure_a) = failure_rx.next().await.unwrap();
                    let (id_b, failure_b) = failure_rx.next().await.unwrap();
                    assert_eq!((id_a, id_b), (a.to_string(), b.to_string()));
                    assert_eq!(reverse_seen_rx.next().await, Some(a.to_string()));
                    assert_eq!(reverse_seen_rx.next().await, Some(b.to_string()));
                    assert_eq!(reverse_notice_rx.next().await, Some(a.to_string()));
                    assert_eq!(reverse_notice_rx.next().await, Some(b.to_string()));
                    let response = connection
                        .send_request(
                            v1::MessageMcpRequest::new(a.clone(), "_test/echo").params(
                                serde_json::Map::from_iter([("value".into(), json!("v1"))]),
                            ),
                        )
                        .block_task()
                        .await?;
                    assert_eq!(
                        serde_json::from_str::<Value>(response.0.get()).unwrap(),
                        json!({"value": "v1"})
                    );
                    connection
                        .send_request(v1::DisconnectMcpRequest::new(a.clone()))
                        .block_task()
                        .await?;
                    assert_eq!(dropped_rx.next().await, Some(a.to_string()));
                    assert!(
                        connection
                            .send_request(v1::DisconnectMcpRequest::new(a))
                            .block_task()
                            .await
                            .is_err()
                    );
                    let response = connection
                        .send_request(v1::MessageMcpRequest::new(b.clone(), "_test/echo").params(
                            serde_json::Map::from_iter([("value".into(), json!("still open"))]),
                        ))
                        .block_task()
                        .await?;
                    assert_eq!(
                        serde_json::from_str::<Value>(response.0.get()).unwrap(),
                        json!({"value": "still open"})
                    );
                    failure_b.send(()).unwrap();
                    assert_eq!(dropped_rx.next().await, Some(b.to_string()));
                    assert!(
                        connection
                            .send_request(v1::DisconnectMcpRequest::new(b))
                            .block_task()
                            .await
                            .is_err()
                    );
                    Ok::<(), Error>(())
                }
                .await;
                result_tx
                    .unbounded_send(result)
                    .map_err(Error::into_internal_error)
            });

        Client
            .builder()
            .connect_with(agent, async move |connection| {
                let session = connection
                    .build_session_cwd()?
                    .with_mcp_server(McpServer::<Agent, _>::new(
                        Server {
                            dropped: dropped_tx,
                            failures: failure_tx,
                            pending_started: pending_started_tx,
                            reverse_seen: reverse_seen_tx,
                        },
                        NullRun,
                    ))?
                    .block_task()
                    .start_session()
                    .await?;
                let result = result_rx
                    .next()
                    .await
                    .expect("v1 scenario did not complete");
                drop(session);
                result
            })
            .await
    };
    tokio::time::timeout(Duration::from_secs(10), test)
        .await
        .expect("v1 native lifecycle timed out")
}
