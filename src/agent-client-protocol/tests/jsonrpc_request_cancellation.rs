#![cfg(feature = "unstable_cancel_request")]

//! Integration tests for `$/cancel_request` support.
//!
//! These tests avoid sleeps by relying on two ordering guarantees:
//!
//! - Messages are delivered in the order they were sent, and each side's
//!   dispatch loop processes incoming messages sequentially. A request/response
//!   round trip therefore acts as a barrier: by the time the response arrives,
//!   every message sent before the request (including any `$/cancel_request`)
//!   has been fully processed by the peer.
//! - Test handlers report observed cancellations through in-process channels,
//!   which the test awaits (with a timeout) instead of sleeping.

use std::sync::{Arc, Mutex};

use agent_client_protocol::{
    Channel, ConnectionTo, Dispatch, Handled, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse,
    Responder, Role, RoleId, SentRequest,
    role::UntypedRole,
    schema::{CancelRequestNotification, ProtocolLevelNotification, RequestId},
};
use expect_test::expect;
use futures::channel::mpsc;
use futures::{AsyncRead, AsyncWrite, StreamExt as _};
use serde::{Deserialize, Serialize};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

fn setup_test_streams() -> (
    impl AsyncRead,
    impl AsyncWrite,
    impl AsyncRead,
    impl AsyncWrite,
) {
    let (client_writer, server_reader) = tokio::io::duplex(4096);
    let (server_writer, client_reader) = tokio::io::duplex(4096);

    let server_reader = server_reader.compat();
    let server_writer = server_writer.compat_write();
    let client_reader = client_reader.compat();
    let client_writer = client_writer.compat_write();

    (server_reader, server_writer, client_reader, client_writer)
}

/// Await the next item on `rx`, panicking instead of hanging if it never
/// arrives.
async fn next_with_timeout<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(tokio::time::Duration::from_secs(10), rx.next())
        .await
        .expect("timed out waiting for channel event")
        .expect("channel closed before expected event")
}

/// Assert that no item is currently buffered on `rx`.
///
/// Callers must first establish an ordering barrier (such as a
/// request/response round trip) that guarantees any erroneously sent
/// notification would already have been observed.
fn assert_no_event<T: std::fmt::Debug>(rx: &mut mpsc::UnboundedReceiver<T>) {
    if let Ok(event) = rx.try_recv() {
        panic!("unexpected event: {event:?}");
    }
}

async fn read_jsonrpc_response_line(
    reader: &mut tokio::io::BufReader<tokio::io::DuplexStream>,
) -> serde_json::Value {
    use tokio::io::AsyncBufReadExt as _;

    let mut line = String::new();
    match tokio::time::timeout(
        tokio::time::Duration::from_secs(10),
        reader.read_line(&mut line),
    )
    .await
    {
        Ok(Ok(0)) | Err(_) => panic!("timed out waiting for JSON-RPC response"),
        Ok(Ok(_)) => serde_json::from_str(line.trim()).expect("response should be valid JSON"),
        Ok(Err(error)) => panic!("failed to read JSON-RPC response line: {error}"),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SimpleRequest {
    message: String,
}

impl JsonRpcMessage for SimpleRequest {
    fn matches_method(method: &str) -> bool {
        method == "simple_method"
    }

    fn method(&self) -> &'static str {
        "simple_method"
    }

    fn to_untyped_message(
        &self,
    ) -> Result<agent_client_protocol::UntypedMessage, agent_client_protocol::Error> {
        agent_client_protocol::UntypedMessage::new(self.method(), self)
    }

    fn parse_message(
        method: &str,
        params: &impl Serialize,
    ) -> Result<Self, agent_client_protocol::Error> {
        if !Self::matches_method(method) {
            return Err(agent_client_protocol::Error::method_not_found());
        }
        agent_client_protocol::util::json_cast_params(params)
    }
}

impl JsonRpcRequest for SimpleRequest {
    type Response = SimpleResponse;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SimpleResponse {
    result: String,
}

impl JsonRpcResponse for SimpleResponse {
    fn into_json(self, _method: &str) -> Result<serde_json::Value, agent_client_protocol::Error> {
        serde_json::to_value(self).map_err(agent_client_protocol::Error::into_internal_error)
    }

    fn from_value(
        _method: &str,
        value: serde_json::Value,
    ) -> Result<Self, agent_client_protocol::Error> {
        agent_client_protocol::util::json_cast(&value)
    }
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct WrappedHost;

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct WrappedCounterpart;

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct WrappedSuccessor;

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct WrappedSuccessorCounterpart;

impl Role for WrappedHost {
    type Counterpart = WrappedCounterpart;

    fn role_id(&self) -> RoleId {
        RoleId::from_singleton(self)
    }

    async fn default_handle_dispatch_from(
        &self,
        message: Dispatch,
        _connection: ConnectionTo<Self>,
    ) -> Result<Handled<Dispatch>, agent_client_protocol::Error> {
        Ok(Handled::No {
            message,
            retry: false,
        })
    }

    fn counterpart(&self) -> Self::Counterpart {
        WrappedCounterpart
    }
}

impl Role for WrappedCounterpart {
    type Counterpart = WrappedHost;

    fn role_id(&self) -> RoleId {
        RoleId::from_singleton(self)
    }

    async fn default_handle_dispatch_from(
        &self,
        message: Dispatch,
        _connection: ConnectionTo<Self>,
    ) -> Result<Handled<Dispatch>, agent_client_protocol::Error> {
        Ok(Handled::No {
            message,
            retry: false,
        })
    }

    fn counterpart(&self) -> Self::Counterpart {
        WrappedHost
    }
}

impl Role for WrappedSuccessor {
    type Counterpart = WrappedSuccessorCounterpart;

    fn role_id(&self) -> RoleId {
        RoleId::from_singleton(self)
    }

    async fn default_handle_dispatch_from(
        &self,
        message: Dispatch,
        _connection: ConnectionTo<Self>,
    ) -> Result<Handled<Dispatch>, agent_client_protocol::Error> {
        Ok(Handled::No {
            message,
            retry: false,
        })
    }

    fn counterpart(&self) -> Self::Counterpart {
        WrappedSuccessorCounterpart
    }
}

impl Role for WrappedSuccessorCounterpart {
    type Counterpart = WrappedSuccessor;

    fn role_id(&self) -> RoleId {
        RoleId::from_singleton(self)
    }

    async fn default_handle_dispatch_from(
        &self,
        message: Dispatch,
        _connection: ConnectionTo<Self>,
    ) -> Result<Handled<Dispatch>, agent_client_protocol::Error> {
        Ok(Handled::No {
            message,
            retry: false,
        })
    }

    fn counterpart(&self) -> Self::Counterpart {
        WrappedSuccessor
    }
}

impl agent_client_protocol::role::HasPeer<WrappedCounterpart> for WrappedCounterpart {
    fn remote_style(&self, _peer: WrappedCounterpart) -> agent_client_protocol::role::RemoteStyle {
        agent_client_protocol::role::RemoteStyle::Counterpart
    }
}

impl agent_client_protocol::role::HasPeer<WrappedSuccessor> for WrappedCounterpart {
    fn remote_style(&self, _peer: WrappedSuccessor) -> agent_client_protocol::role::RemoteStyle {
        agent_client_protocol::role::RemoteStyle::Successor
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unhandled_protocol_level_notifications_are_ignored() {
    use tokio::io::{AsyncWriteExt, BufReader};
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local
        .run_until(async {
            let (mut client_writer, server_reader) = tokio::io::duplex(4096);
            let (server_writer, client_reader) = tokio::io::duplex(4096);

            let server_transport = agent_client_protocol::ByteStreams::new(
                server_writer.compat_write(),
                server_reader.compat(),
            );
            let server = UntypedRole.builder().on_receive_request(
                async |request: SimpleRequest,
                       responder: Responder<SimpleResponse>,
                       _connection: ConnectionTo<UntypedRole>| {
                    responder.respond(SimpleResponse {
                        result: format!("echo: {}", request.message),
                    })
                },
                agent_client_protocol::on_receive_request!(),
            );

            tokio::task::spawn_local(async move {
                if let Err(error) = server.connect_to(server_transport).await {
                    panic!("server should stay alive: {error:?}");
                }
            });

            let mut client_reader = BufReader::new(client_reader);

            client_writer
                .write_all(
                    br#"{"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":"req-1"}}
"#,
                )
                .await
                .unwrap();
            client_writer.flush().await.unwrap();

            // The server processes messages in order: a response to this
            // request proves the unknown `$/` notification before it was
            // ignored without erroring or closing the connection.
            client_writer
                .write_all(
                    br#"{"jsonrpc":"2.0","id":2,"method":"simple_method","params":{"message":"after cancel"}}
"#,
                )
                .await
                .unwrap();
            client_writer.flush().await.unwrap();

            let response = read_jsonrpc_response_line(&mut client_reader).await;
            expect![[r#"
                {
                  "jsonrpc": "2.0",
                  "result": {
                    "result": "echo: after cancel"
                  },
                  "id": 2
                }"#]]
            .assert_eq(&serde_json::to_string_pretty(&response).unwrap());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn unhandled_wrapped_protocol_level_notifications_are_ignored() {
    use tokio::io::{AsyncWriteExt, BufReader};
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local
        .run_until(async {
            let (mut client_writer, server_reader) = tokio::io::duplex(4096);
            let (server_writer, client_reader) = tokio::io::duplex(4096);

            let server_transport = agent_client_protocol::ByteStreams::new(
                server_writer.compat_write(),
                server_reader.compat(),
            );
            let server = WrappedHost
                .builder()
                .on_receive_notification_from(
                    WrappedSuccessor,
                    async |cancel: CancelRequestNotification,
                           cx: ConnectionTo<WrappedCounterpart>| {
                        Ok::<_, agent_client_protocol::Error>(Handled::No {
                            message: (cancel, cx),
                            retry: false,
                        })
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .on_receive_request(
                    async |request: SimpleRequest,
                           responder: Responder<SimpleResponse>,
                           _connection: ConnectionTo<WrappedCounterpart>| {
                        responder.respond(SimpleResponse {
                            result: format!("echo: {}", request.message),
                        })
                    },
                    agent_client_protocol::on_receive_request!(),
                );

            tokio::task::spawn_local(async move {
                if let Err(error) = server.connect_to(server_transport).await {
                    panic!("server should stay alive: {error:?}");
                }
            });

            let mut client_reader = BufReader::new(client_reader);

            client_writer
                .write_all(
                    br#"{"jsonrpc":"2.0","method":"_proxy/successor","params":{"method":"$/cancel_request","params":{"requestId":"req-1"}}}
"#,
                )
                .await
                .unwrap();
            client_writer.flush().await.unwrap();

            client_writer
                .write_all(
                    br#"{"jsonrpc":"2.0","id":2,"method":"simple_method","params":{"message":"after wrapped cancel"}}
"#,
                )
                .await
                .unwrap();
            client_writer.flush().await.unwrap();

            let response = read_jsonrpc_response_line(&mut client_reader).await;
            expect![[r#"
                {
                  "jsonrpc": "2.0",
                  "result": {
                    "result": "echo: after wrapped cancel"
                  },
                  "id": 2
                }"#]]
            .assert_eq(&serde_json::to_string_pretty(&response).unwrap());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_request_notification_can_be_sent_and_handled() {
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local
        .run_until(async {
            let (cancel_tx, mut cancel_rx) = mpsc::unbounded();

            let (server_reader, server_writer, client_reader, client_writer) = setup_test_streams();
            let server_transport =
                agent_client_protocol::ByteStreams::new(server_writer, server_reader);
            let server = UntypedRole.builder().on_receive_notification(
                async move |notification: CancelRequestNotification,
                            _connection: ConnectionTo<UntypedRole>| {
                    cancel_tx.unbounded_send(notification.request_id).unwrap();
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            );

            tokio::task::spawn_local(async move {
                if let Err(error) = server.connect_to(server_transport).await {
                    panic!("server should stay alive: {error:?}");
                }
            });

            let client_transport =
                agent_client_protocol::ByteStreams::new(client_writer, client_reader);
            let received = UntypedRole
                .builder()
                .connect_with(client_transport, async |cx| {
                    cx.send_cancel_request("request-42".to_string())?;
                    Ok(next_with_timeout(&mut cancel_rx).await)
                })
                .await
                .unwrap();

            assert_eq!(received, RequestId::Str("request-42".into()));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn sent_request_can_send_cancellation_for_its_id() {
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local
        .run_until(async {
            let (cancel_tx, mut cancel_rx) = mpsc::unbounded();

            let (server_reader, server_writer, client_reader, client_writer) = setup_test_streams();
            let server_transport =
                agent_client_protocol::ByteStreams::new(server_writer, server_reader);
            let server = UntypedRole
                .builder()
                .on_receive_request(
                    async |_request: SimpleRequest,
                           _responder: Responder<SimpleResponse>,
                           _connection: ConnectionTo<UntypedRole>| { Ok(()) },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_notification(
                    async move |notification: CancelRequestNotification,
                                _connection: ConnectionTo<UntypedRole>| {
                        cancel_tx.unbounded_send(notification.request_id).unwrap();
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                );

            tokio::task::spawn_local(async move {
                if let Err(error) = server.connect_to(server_transport).await {
                    panic!("server should stay alive: {error:?}");
                }
            });

            let client_transport =
                agent_client_protocol::ByteStreams::new(client_writer, client_reader);
            let (expected_id, received) = UntypedRole
                .builder()
                .connect_with(client_transport, async |cx| {
                    let request: SentRequest<SimpleResponse> = cx.send_request(SimpleRequest {
                        message: "slow".into(),
                    });
                    let expected_id = request.id();
                    request.cancel()?;
                    let received = next_with_timeout(&mut cancel_rx).await;
                    Ok((expected_id, received))
                })
                .await
                .unwrap();

            assert_eq!(serde_json::to_value(received).unwrap(), expected_id);
            assert_no_event(&mut cancel_rx);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn dropped_sent_request_sends_cancellation_for_its_id() {
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local
        .run_until(async {
            let (cancel_tx, mut cancel_rx) = mpsc::unbounded();

            let (server_reader, server_writer, client_reader, client_writer) = setup_test_streams();
            let server_transport =
                agent_client_protocol::ByteStreams::new(server_writer, server_reader);
            let server = UntypedRole
                .builder()
                .on_receive_request(
                    async |_request: SimpleRequest,
                           _responder: Responder<SimpleResponse>,
                           _connection: ConnectionTo<UntypedRole>| { Ok(()) },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_notification(
                    async move |notification: CancelRequestNotification,
                                _connection: ConnectionTo<UntypedRole>| {
                        cancel_tx.unbounded_send(notification.request_id).unwrap();
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                );

            tokio::task::spawn_local(async move {
                if let Err(error) = server.connect_to(server_transport).await {
                    panic!("server should stay alive: {error:?}");
                }
            });

            let client_transport =
                agent_client_protocol::ByteStreams::new(client_writer, client_reader);
            let (expected_id, received) = UntypedRole
                .builder()
                .connect_with(client_transport, async |cx| {
                    let request: SentRequest<SimpleResponse> = cx.send_request(SimpleRequest {
                        message: "abandoned".into(),
                    });
                    let expected_id = request.id();
                    drop(request);
                    let received = next_with_timeout(&mut cancel_rx).await;
                    Ok((expected_id, received))
                })
                .await
                .unwrap();

            assert_eq!(serde_json::to_value(received).unwrap(), expected_id);
            assert_no_event(&mut cancel_rx);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn late_response_after_dropped_sent_request_does_not_close_connection() {
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local
        .run_until(async {
            let (cancel_tx, mut cancel_rx) = mpsc::unbounded();
            // The responder for the abandoned request, held by the server
            // until the cancellation notification arrives.
            let pending_responder: Arc<Mutex<Option<Responder<SimpleResponse>>>> =
                Arc::new(Mutex::new(None));

            let (server_reader, server_writer, client_reader, client_writer) = setup_test_streams();
            let server_transport =
                agent_client_protocol::ByteStreams::new(server_writer, server_reader);
            let server = UntypedRole
                .builder()
                .on_receive_request(
                    {
                        let pending_responder = pending_responder.clone();
                        async move |request: SimpleRequest,
                                    responder: Responder<SimpleResponse>,
                                    _connection: ConnectionTo<UntypedRole>| {
                            if request.message == "late" {
                                *pending_responder.lock().unwrap() = Some(responder);
                                return Ok(());
                            }

                            responder.respond(SimpleResponse {
                                result: format!("echo: {}", request.message),
                            })
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_notification(
                    {
                        let pending_responder = pending_responder.clone();
                        async move |notification: CancelRequestNotification,
                                    _connection: ConnectionTo<UntypedRole>| {
                            // Ignore the cancellation and answer the abandoned
                            // request anyway: the client must tolerate this.
                            if let Some(responder) = pending_responder.lock().unwrap().take() {
                                responder.respond(SimpleResponse {
                                    result: "late response".into(),
                                })?;
                            }
                            cancel_tx.unbounded_send(notification.request_id).unwrap();
                            Ok(())
                        }
                    },
                    agent_client_protocol::on_receive_notification!(),
                );

            tokio::task::spawn_local(async move {
                if let Err(error) = server.connect_to(server_transport).await {
                    panic!("server should stay alive: {error:?}");
                }
            });

            let client_transport =
                agent_client_protocol::ByteStreams::new(client_writer, client_reader);
            let (expected_id, received, response) = UntypedRole
                .builder()
                .connect_with(client_transport, async |cx| {
                    let request: SentRequest<SimpleResponse> = cx.send_request(SimpleRequest {
                        message: "late".into(),
                    });
                    let expected_id = request.id();
                    drop(request);

                    let received = next_with_timeout(&mut cancel_rx).await;

                    // The server sent the late response before answering this
                    // follow-up, so a successful round trip proves the late
                    // response for the dropped request was routed without
                    // closing the connection.
                    let response = cx
                        .send_request(SimpleRequest {
                            message: "after late".into(),
                        })
                        .block_task()
                        .await?;
                    Ok((expected_id, received, response))
                })
                .await
                .unwrap();

            assert_eq!(response.result, "echo: after late");
            assert_eq!(serde_json::to_value(received).unwrap(), expected_id);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn response_buffered_before_drop_disarms_auto_cancellation() {
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local
        .run_until(async {
            let (cancel_tx, mut cancel_rx) = mpsc::unbounded();

            let (server_reader, server_writer, client_reader, client_writer) = setup_test_streams();
            let server_transport =
                agent_client_protocol::ByteStreams::new(server_writer, server_reader);
            let server = UntypedRole
                .builder()
                .on_receive_request(
                    async |request: SimpleRequest,
                           responder: Responder<SimpleResponse>,
                           _connection: ConnectionTo<UntypedRole>| {
                        responder.respond(SimpleResponse {
                            result: format!("echo: {}", request.message),
                        })
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_notification(
                    async move |notification: CancelRequestNotification,
                                _connection: ConnectionTo<UntypedRole>| {
                        cancel_tx.unbounded_send(notification.request_id).unwrap();
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                );

            tokio::task::spawn_local(async move {
                if let Err(error) = server.connect_to(server_transport).await {
                    panic!("server should stay alive: {error:?}");
                }
            });

            let client_transport =
                agent_client_protocol::ByteStreams::new(client_writer, client_reader);
            let response = UntypedRole
                .builder()
                .connect_with(client_transport, async |cx| {
                    let request: SentRequest<SimpleResponse> = cx.send_request(SimpleRequest {
                        message: "buffered".into(),
                    });

                    // The server answers requests in order, so once this round
                    // trip completes, the response to `buffered` has already
                    // been routed into the unconsumed request handle above,
                    // disarming its auto-cancellation.
                    let barrier = cx
                        .send_request(SimpleRequest {
                            message: "barrier".into(),
                        })
                        .block_task()
                        .await?;
                    assert_eq!(barrier.result, "echo: barrier");

                    drop(request);

                    // Another round trip: any cancellation sent by the drop
                    // above would reach the server before this request.
                    cx.send_request(SimpleRequest {
                        message: "after buffered".into(),
                    })
                    .block_task()
                    .await
                })
                .await
                .unwrap();

            assert_eq!(response.result, "echo: after buffered");
            assert_no_event(&mut cancel_rx);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn completed_sent_request_does_not_send_cancellation_on_drop() {
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local
        .run_until(async {
            let (cancel_tx, mut cancel_rx) = mpsc::unbounded();

            let (server_reader, server_writer, client_reader, client_writer) = setup_test_streams();
            let server_transport =
                agent_client_protocol::ByteStreams::new(server_writer, server_reader);
            let server = UntypedRole
                .builder()
                .on_receive_request(
                    async |request: SimpleRequest,
                           responder: Responder<SimpleResponse>,
                           _connection: ConnectionTo<UntypedRole>| {
                        responder.respond(SimpleResponse {
                            result: format!("echo: {}", request.message),
                        })
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_notification(
                    async move |notification: CancelRequestNotification,
                                _connection: ConnectionTo<UntypedRole>| {
                        cancel_tx.unbounded_send(notification.request_id).unwrap();
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                );

            tokio::task::spawn_local(async move {
                if let Err(error) = server.connect_to(server_transport).await {
                    panic!("server should stay alive: {error:?}");
                }
            });

            let client_transport =
                agent_client_protocol::ByteStreams::new(client_writer, client_reader);
            let response = UntypedRole
                .builder()
                .connect_with(client_transport, async |cx| {
                    let response = cx
                        .send_request(SimpleRequest {
                            message: "complete".into(),
                        })
                        .block_task()
                        .await?;

                    // Barrier round trip: any cancellation erroneously sent
                    // when the completed request handle was dropped would
                    // reach the server before this request.
                    let barrier = cx
                        .send_request(SimpleRequest {
                            message: "barrier".into(),
                        })
                        .block_task()
                        .await?;
                    assert_eq!(barrier.result, "echo: barrier");

                    Ok(response)
                })
                .await
                .unwrap();

            assert_eq!(response.result, "echo: complete");
            assert_no_event(&mut cancel_rx);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn forward_response_to_propagates_cancellation_to_downstream_request() {
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local
        .run_until(async {
            let (backend_cancel_tx, mut backend_cancel_rx) = mpsc::unbounded();

            let (backend_for_proxy, backend_for_server) = Channel::duplex();
            let (backend_connection_tx, backend_connection_rx) =
                futures::channel::oneshot::channel();

            tokio::task::spawn_local(async move {
                let result = UntypedRole
                    .builder()
                    .connect_with(backend_for_proxy, async |connection| {
                        drop(backend_connection_tx.send(connection.clone()));
                        std::future::pending::<Result<(), agent_client_protocol::Error>>().await
                    })
                    .await;
                if let Err(error) = result {
                    panic!("proxy-to-backend connection should stay alive: {error:?}");
                }
            });

            let backend_server = UntypedRole
                .builder()
                .on_receive_request(
                    async |_request: SimpleRequest,
                           _responder: Responder<SimpleResponse>,
                           _connection: ConnectionTo<UntypedRole>| { Ok(()) },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_notification(
                    async move |notification: CancelRequestNotification,
                                _connection: ConnectionTo<UntypedRole>| {
                        backend_cancel_tx
                            .unbounded_send(notification.request_id)
                            .unwrap();
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                );

            tokio::task::spawn_local(async move {
                if let Err(error) = backend_server.connect_to(backend_for_server).await {
                    panic!("backend server should stay alive: {error:?}");
                }
            });

            let backend_connection = backend_connection_rx
                .await
                .expect("backend connection should start");

            let (server_reader, server_writer, client_reader, client_writer) = setup_test_streams();
            let proxy_transport =
                agent_client_protocol::ByteStreams::new(server_writer, server_reader);
            let proxy = UntypedRole.builder().on_receive_request(
                {
                    let backend_connection = backend_connection.clone();
                    async move |request: SimpleRequest,
                                responder: Responder<SimpleResponse>,
                                _connection: ConnectionTo<UntypedRole>| {
                        backend_connection
                            .send_request(request)
                            .forward_response_to(responder)?;
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_request!(),
            );

            tokio::task::spawn_local(async move {
                if let Err(error) = proxy.connect_to(proxy_transport).await {
                    panic!("proxy should stay alive: {error:?}");
                }
            });

            let client_transport =
                agent_client_protocol::ByteStreams::new(client_writer, client_reader);
            UntypedRole
                .builder()
                .connect_with(client_transport, async |connection| {
                    let request: SentRequest<SimpleResponse> =
                        connection.send_request(SimpleRequest {
                            message: "cancel downstream".into(),
                        });
                    request.cancel()?;
                    // Wait until the proxy has forwarded the cancellation all
                    // the way to the backend.
                    next_with_timeout(&mut backend_cancel_rx).await;
                    Ok(())
                })
                .await
                .unwrap();

            assert_no_event(&mut backend_cancel_rx);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn request_handler_can_observe_cancellation_from_responder() {
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local
        .run_until(async {
            let (server_reader, server_writer, client_reader, client_writer) = setup_test_streams();
            let server_transport =
                agent_client_protocol::ByteStreams::new(server_writer, server_reader);
            let server = UntypedRole.builder().on_receive_request(
                async |_request: SimpleRequest,
                       responder: Responder<SimpleResponse>,
                       connection: ConnectionTo<UntypedRole>| {
                    let cancellation = responder.cancellation();
                    assert!(!cancellation.is_cancelled());

                    connection.spawn(async move {
                        let response = cancellation
                            .run_until_cancelled(futures::future::pending::<
                                Result<SimpleResponse, agent_client_protocol::Error>,
                            >())
                            .await;
                        assert!(cancellation.is_cancelled());
                        responder.respond_with_result(response)
                    })?;

                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            );

            tokio::task::spawn_local(async move {
                if let Err(error) = server.connect_to(server_transport).await {
                    panic!("server should stay alive: {error:?}");
                }
            });

            let client_transport =
                agent_client_protocol::ByteStreams::new(client_writer, client_reader);
            let error = UntypedRole
                .builder()
                .connect_with(client_transport, async |cx| {
                    let request: SentRequest<SimpleResponse> = cx.send_request(SimpleRequest {
                        message: "cancel me".into(),
                    });
                    request.cancel()?;
                    Ok(request
                        .block_task()
                        .await
                        .expect_err("request should be cancelled"))
                })
                .await
                .unwrap();

            assert_eq!(i32::from(error.code), -32800);
            assert_eq!(error.message, "Request cancelled");
        })
        .await;
}

#[test]
fn protocol_level_notification_and_cancelled_error_code_are_typed() {
    let notification = ProtocolLevelNotification::parse_message(
        "$/cancel_request",
        &serde_json::json!({ "requestId": "req-1" }),
    )
    .unwrap();
    assert_eq!(notification.method(), "$/cancel_request");

    let error = agent_client_protocol::Error::request_cancelled();
    assert_eq!(i32::from(error.code), -32800);
    assert_eq!(error.message, "Request cancelled");
}
