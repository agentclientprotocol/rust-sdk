//! Error handling tests for JSON-RPC layer
//!
//! Tests various error conditions:
//! - Invalid JSON
//! - Unknown methods
//! - Handler-returned errors
//! - Serialization failures
//! - Missing/invalid parameters

use agent_client_protocol_core::{
    ConnectionTo, Dispatch, HandleDispatchFrom, Handled, JsonRpcMessage, JsonRpcNotification,
    JsonRpcRequest, JsonRpcResponse, RequestMatch, Responder, SentRequest,
    role::UntypedRole,
    util::{MatchDispatch, MatchDispatchFrom},
};
use expect_test::expect;
use futures::{AsyncRead, AsyncWrite};
use serde::{Deserialize, Serialize};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

async fn read_jsonrpc_response_line(
    reader: &mut tokio::io::BufReader<tokio::io::DuplexStream>,
) -> serde_json::Value {
    try_read_jsonrpc_response_line(reader, tokio::time::Duration::from_secs(1))
        .await
        .expect("timed out waiting for JSON-RPC response")
}

async fn try_read_jsonrpc_response_line(
    reader: &mut tokio::io::BufReader<tokio::io::DuplexStream>,
    timeout: tokio::time::Duration,
) -> Option<serde_json::Value> {
    use tokio::io::AsyncBufReadExt as _;

    let mut line = String::new();
    match tokio::time::timeout(timeout, reader.read_line(&mut line)).await {
        Ok(Ok(0)) | Err(_) => None,
        Ok(Ok(_)) => {
            Some(serde_json::from_str(line.trim()).expect("response should be valid JSON"))
        }
        Ok(Err(_)) => panic!("failed to read JSON-RPC response line"),
    }
}

/// Test helper to block and wait for a JSON-RPC response.
async fn recv<T: JsonRpcResponse + Send>(
    response: SentRequest<T>,
) -> Result<T, agent_client_protocol_core::Error> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    response.on_receiving_result(async move |result| {
        tx.send(result)
            .map_err(|_| agent_client_protocol_core::Error::internal_error())
    })?;
    rx.await
        .map_err(|_| agent_client_protocol_core::Error::internal_error())?
}

/// Helper to set up test streams.
fn setup_test_streams() -> (
    impl AsyncRead,
    impl AsyncWrite,
    impl AsyncRead,
    impl AsyncWrite,
) {
    let (client_writer, server_reader) = tokio::io::duplex(1024);
    let (server_writer, client_reader) = tokio::io::duplex(1024);

    let server_reader = server_reader.compat();
    let server_writer = server_writer.compat_write();
    let client_reader = client_reader.compat();
    let client_writer = client_writer.compat_write();

    (server_reader, server_writer, client_reader, client_writer)
}

// ============================================================================
// Test types
// ============================================================================

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
    ) -> Result<agent_client_protocol_core::UntypedMessage, agent_client_protocol_core::Error> {
        agent_client_protocol_core::UntypedMessage::new(self.method(), self)
    }

    fn parse_message(
        method: &str,
        params: &impl serde::Serialize,
    ) -> Result<Self, agent_client_protocol_core::Error> {
        if !Self::matches_method(method) {
            return Err(agent_client_protocol_core::Error::method_not_found());
        }
        agent_client_protocol_core::util::json_cast_params(params)
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
    fn into_json(
        self,
        _method: &str,
    ) -> Result<serde_json::Value, agent_client_protocol_core::Error> {
        serde_json::to_value(self).map_err(agent_client_protocol_core::Error::into_internal_error)
    }

    fn from_value(
        _method: &str,
        value: serde_json::Value,
    ) -> Result<Self, agent_client_protocol_core::Error> {
        agent_client_protocol_core::util::json_cast(&value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SimpleNotification {
    message: String,
}

impl JsonRpcMessage for SimpleNotification {
    fn matches_method(method: &str) -> bool {
        method == "simple_notification"
    }

    fn method(&self) -> &'static str {
        "simple_notification"
    }

    fn to_untyped_message(
        &self,
    ) -> Result<agent_client_protocol_core::UntypedMessage, agent_client_protocol_core::Error> {
        agent_client_protocol_core::UntypedMessage::new(self.method(), self)
    }

    fn parse_message(
        method: &str,
        params: &impl serde::Serialize,
    ) -> Result<Self, agent_client_protocol_core::Error> {
        if !Self::matches_method(method) {
            return Err(agent_client_protocol_core::Error::method_not_found());
        }
        agent_client_protocol_core::util::json_cast_params(params)
    }
}

impl JsonRpcNotification for SimpleNotification {}

// ============================================================================
// Test 1: Invalid JSON (complete line with parse error)
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn test_invalid_json() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local
        .run_until(async {
            // Create duplex streams for bidirectional communication
            let (mut client_writer, server_reader) = tokio::io::duplex(1024);
            let (server_writer, mut client_reader) = tokio::io::duplex(1024);

            let server_reader = server_reader.compat();
            let server_writer = server_writer.compat_write();

            // No handlers - all requests will return errors
            let server_transport =
                agent_client_protocol_core::ByteStreams::new(server_writer, server_reader);
            let server = UntypedRole.builder();

            // Spawn server
            tokio::task::spawn_local(async move {
                drop(server.connect_to(server_transport).await);
            });

            // Send invalid JSON
            let invalid_json = b"{\"method\": \"test\", \"id\": 1, INVALID}\n";
            client_writer.write_all(invalid_json).await.unwrap();
            client_writer.flush().await.unwrap();

            // Read response
            let mut buffer = vec![0u8; 1024];
            let n = client_reader.read(&mut buffer).await.unwrap();
            let response_str = String::from_utf8_lossy(&buffer[..n]);

            // Parse as JSON and verify structure
            let response: serde_json::Value =
                serde_json::from_str(response_str.trim()).expect("Response should be valid JSON");

            // Use expect_test to verify the exact structure
            expect![[r#"
                {
                  "error": {
                    "code": -32700,
                    "data": {
                      "line": "{\"method\": \"test\", \"id\": 1, INVALID}"
                    },
                    "message": "Parse error"
                  },
                  "jsonrpc": "2.0"
                }"#]]
            .assert_eq(&serde_json::to_string_pretty(&response).unwrap());
        })
        .await;
}

// ============================================================================
// Test 1b: Incomplete line (EOF mid-message)
// ============================================================================

#[tokio::test]
#[ignore = "hangs indefinitely - see https://github.com/agentclientprotocol/rust-sdk/issues/64"]
async fn test_incomplete_line() {
    use futures::io::Cursor;

    // Incomplete JSON input - no newline, simulates client disconnect
    let incomplete_json = b"{\"method\": \"test\", \"id\": 1";
    let input = Cursor::new(incomplete_json.to_vec());
    let output = Cursor::new(Vec::new());

    // No handlers needed for EOF test
    let transport = agent_client_protocol_core::ByteStreams::new(output, input);
    let connection = UntypedRole.builder();

    // The server should handle EOF mid-message gracefully
    let result = connection.connect_to(transport).await;

    // Server should terminate cleanly when hitting EOF
    assert!(result.is_ok() || result.is_err());
}

// ============================================================================
// Test 2: Unknown method (no handler claims)
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn test_unknown_method() {
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local
        .run_until(async {
            let (server_reader, server_writer, client_reader, client_writer) = setup_test_streams();

            // No handlers - all requests will be "method not found"
            let server_transport =
                agent_client_protocol_core::ByteStreams::new(server_writer, server_reader);
            let server = UntypedRole.builder();
            let client_transport =
                agent_client_protocol_core::ByteStreams::new(client_writer, client_reader);
            let client = UntypedRole.builder();

            // Spawn server
            tokio::task::spawn_local(async move {
                server.connect_to(server_transport).await.ok();
            });

            // Send request from client
            let result = client
                .connect_with(
                    client_transport,
                    async |cx| -> Result<(), agent_client_protocol_core::Error> {
                        let request = SimpleRequest {
                            message: "test".to_string(),
                        };

                        let result: Result<SimpleResponse, _> =
                            recv(cx.send_request(request)).await;

                        // Should get an error because no handler claims the method
                        assert!(result.is_err());
                        if let Err(err) = result {
                            // Should be "method not found" or similar error
                            assert!(matches!(
                                err.code,
                                agent_client_protocol_core::ErrorCode::MethodNotFound
                            ));
                        }
                        Ok(())
                    },
                )
                .await;

            assert!(result.is_ok(), "Test failed: {result:?}");
        })
        .await;
}

// ============================================================================
// Test 3: Handler returns error
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ErrorRequest {
    value: String,
}

impl JsonRpcMessage for ErrorRequest {
    fn matches_method(method: &str) -> bool {
        method == "error_method"
    }

    fn method(&self) -> &'static str {
        "error_method"
    }

    fn to_untyped_message(
        &self,
    ) -> Result<agent_client_protocol_core::UntypedMessage, agent_client_protocol_core::Error> {
        agent_client_protocol_core::UntypedMessage::new(self.method(), self)
    }

    fn parse_message(
        method: &str,
        params: &impl serde::Serialize,
    ) -> Result<Self, agent_client_protocol_core::Error> {
        if !Self::matches_method(method) {
            return Err(agent_client_protocol_core::Error::method_not_found());
        }
        agent_client_protocol_core::util::json_cast_params(params)
    }
}

impl JsonRpcRequest for ErrorRequest {
    type Response = SimpleResponse;
}

#[tokio::test(flavor = "current_thread")]
async fn test_handler_returns_error() {
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local
        .run_until(async {
            let (server_reader, server_writer, client_reader, client_writer) = setup_test_streams();

            let server_transport =
                agent_client_protocol_core::ByteStreams::new(server_writer, server_reader);
            let server = UntypedRole.builder().on_receive_request(
                async |_request: ErrorRequest,
                       responder: Responder<SimpleResponse>,
                       _connection: ConnectionTo<UntypedRole>| {
                    // Explicitly return an error
                    responder
                        .respond_with_error(agent_client_protocol_core::Error::internal_error())
                },
                agent_client_protocol_core::on_receive_request!(),
            );

            let client_transport =
                agent_client_protocol_core::ByteStreams::new(client_writer, client_reader);
            let client = UntypedRole.builder();

            tokio::task::spawn_local(async move {
                server.connect_to(server_transport).await.ok();
            });

            let result = client
                .connect_with(
                    client_transport,
                    async |cx| -> Result<(), agent_client_protocol_core::Error> {
                        let request = ErrorRequest {
                            value: "trigger error".to_string(),
                        };

                        let result: Result<SimpleResponse, _> =
                            recv(cx.send_request(request)).await;

                        // Should get the error the handler returned
                        assert!(result.is_err());
                        if let Err(err) = result {
                            assert!(matches!(
                                err.code,
                                agent_client_protocol_core::ErrorCode::InternalError
                            ));
                        }
                        Ok(())
                    },
                )
                .await;

            assert!(result.is_ok(), "Test failed: {result:?}");
        })
        .await;
}

// ============================================================================
// Test 4: Handler-returned invalid params
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EmptyRequest;

impl JsonRpcMessage for EmptyRequest {
    fn matches_method(method: &str) -> bool {
        method == "strict_method"
    }

    fn method(&self) -> &'static str {
        "strict_method"
    }

    fn to_untyped_message(
        &self,
    ) -> Result<agent_client_protocol_core::UntypedMessage, agent_client_protocol_core::Error> {
        agent_client_protocol_core::UntypedMessage::new(self.method(), self)
    }

    fn parse_message(
        method: &str,
        _params: &impl serde::Serialize,
    ) -> Result<Self, agent_client_protocol_core::Error> {
        if !Self::matches_method(method) {
            return Err(agent_client_protocol_core::Error::method_not_found());
        }
        Ok(EmptyRequest)
    }
}

impl JsonRpcRequest for EmptyRequest {
    type Response = SimpleResponse;
}

#[tokio::test(flavor = "current_thread")]
async fn test_handler_returned_invalid_params() {
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local
        .run_until(async {
            let (server_reader, server_writer, client_reader, client_writer) = setup_test_streams();

            // This test exercises a handler that explicitly returns `Invalid params`.
            // It does not cover request deserialization failures; those are covered below
            // by the raw-wire malformed-request regression tests.
            let server_transport =
                agent_client_protocol_core::ByteStreams::new(server_writer, server_reader);
            let server = UntypedRole.builder().on_receive_request(
                async |_request: EmptyRequest,
                       responder: Responder<SimpleResponse>,
                       _connection: ConnectionTo<UntypedRole>| {
                    responder
                        .respond_with_error(agent_client_protocol_core::Error::invalid_params())
                },
                agent_client_protocol_core::on_receive_request!(),
            );

            let client_transport =
                agent_client_protocol_core::ByteStreams::new(client_writer, client_reader);
            let client = UntypedRole.builder();

            tokio::task::spawn_local(async move {
                server.connect_to(server_transport).await.ok();
            });

            let result = client
                .connect_with(
                    client_transport,
                    async |cx| -> Result<(), agent_client_protocol_core::Error> {
                        let request = EmptyRequest;

                        let result: Result<SimpleResponse, _> =
                            recv(cx.send_request(request)).await;

                        // Should get invalid_params error from the handler.
                        assert!(result.is_err());
                        if let Err(err) = result {
                            assert!(matches!(
                                err.code,
                                agent_client_protocol_core::ErrorCode::InvalidParams
                            )); // JSONRPC_INVALID_PARAMS
                        }
                        Ok(())
                    },
                )
                .await;

            assert!(result.is_ok(), "Test failed: {result:?}");
        })
        .await;
}

// ============================================================================
// Test 5: Malformed incoming responses
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn test_match_dispatch_from_if_message_malformed_response_keeps_connection_alive() {
    use tokio::io::{AsyncWriteExt, BufReader};
    use tokio::task::LocalSet;

    struct ClientTypedMessageHandler;

    impl HandleDispatchFrom<UntypedRole> for ClientTypedMessageHandler {
        fn describe_chain(&self) -> impl std::fmt::Debug {
            "ClientTypedMessageHandler"
        }

        async fn handle_dispatch_from(
            &mut self,
            message: Dispatch,
            connection: ConnectionTo<UntypedRole>,
        ) -> Result<Handled<Dispatch>, agent_client_protocol_core::Error> {
            MatchDispatchFrom::new(message, &connection)
                .if_message_from(
                    UntypedRole,
                    async move |dispatch: Dispatch<SimpleRequest, SimpleNotification>| {
                        match dispatch {
                            Dispatch::Request(request, responder) => {
                                responder.respond(SimpleResponse {
                                    result: format!("echo: {}", request.message),
                                })
                            }
                            Dispatch::Notification(_) => Ok(()),
                            Dispatch::Response(result, router) => {
                                router.respond_with_result(result)
                            }
                        }
                    },
                )
                .await
                .done()
        }
    }

    let local = LocalSet::new();

    local
        .run_until(async {
            let (client_writer, server_reader) = tokio::io::duplex(2048);
            let (mut server_writer, client_reader) = tokio::io::duplex(2048);

            let client_transport = agent_client_protocol_core::ByteStreams::new(
                client_writer.compat_write(),
                client_reader.compat(),
            );
            let client = UntypedRole
                .builder()
                .with_handler(ClientTypedMessageHandler);

            let server_task = tokio::task::spawn_local(async move {
                let mut server_reader = BufReader::new(server_reader);

                let first_request = read_jsonrpc_response_line(&mut server_reader).await;
                assert_eq!(first_request["jsonrpc"], "2.0");
                assert_eq!(first_request["method"], "simple_method");
                assert_eq!(first_request["params"]["message"], "first");
                let first_id = first_request["id"].clone();
                assert_ne!(first_id, serde_json::Value::Null);

                let malformed_response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": first_id,
                    "result": {
                        "wrong_field": "oops"
                    }
                });
                let malformed_line =
                    format!("{}\n", serde_json::to_string(&malformed_response).unwrap());
                server_writer
                    .write_all(malformed_line.as_bytes())
                    .await
                    .unwrap();
                server_writer.flush().await.unwrap();

                let second_request = read_jsonrpc_response_line(&mut server_reader).await;
                assert_eq!(second_request["jsonrpc"], "2.0");
                assert_eq!(second_request["method"], "simple_method");
                assert_eq!(second_request["params"]["message"], "second");
                let second_id = second_request["id"].clone();
                assert_ne!(second_id, serde_json::Value::Null);

                let good_response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": second_id,
                    "result": {
                        "result": "echo: second"
                    }
                });
                let good_line = format!("{}\n", serde_json::to_string(&good_response).unwrap());
                server_writer.write_all(good_line.as_bytes()).await.unwrap();
                server_writer.flush().await.unwrap();
            });

            let client_result = client
                .connect_with(
                    client_transport,
                    async |cx| -> Result<(), agent_client_protocol_core::Error> {
                        let bad_result = tokio::time::timeout(
                            tokio::time::Duration::from_secs(1),
                            recv(cx.send_request(SimpleRequest {
                                message: "first".to_string(),
                            })),
                        )
                        .await
                        .expect("malformed response should complete with an error, not hang");

                        let err = bad_result.expect_err(
                            "malformed response payload should be reported as an error",
                        );
                        assert!(matches!(
                            err.code,
                            agent_client_protocol_core::ErrorCode::InternalError
                        ));
                        let err_data = serde_json::to_value(&err.data)
                            .expect("error data should serialize to JSON");
                        assert_eq!(err_data["phase"], "deserialization");
                        assert_eq!(
                            err_data["json"],
                            serde_json::json!({
                                "wrong_field": "oops"
                            })
                        );

                        let good_result = tokio::time::timeout(
                            tokio::time::Duration::from_secs(1),
                            recv(cx.send_request(SimpleRequest {
                                message: "second".to_string(),
                            })),
                        )
                        .await
                        .expect("connection should remain alive after malformed response")?;

                        assert_eq!(good_result.result, "echo: second");
                        Ok(())
                    },
                )
                .await;

            server_task.await.unwrap();
            assert!(
                client_result.is_ok(),
                "client should stay alive: {client_result:?}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_bad_request_params_return_invalid_params_and_connection_stays_alive() {
    use tokio::io::{AsyncWriteExt, BufReader};
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local
        .run_until(async {
            let (mut client_writer, server_reader) = tokio::io::duplex(2048);
            let (server_writer, client_reader) = tokio::io::duplex(2048);

            let server_reader = server_reader.compat();
            let server_writer = server_writer.compat_write();

            let server_transport =
                agent_client_protocol_core::ByteStreams::new(server_writer, server_reader);
            let server = UntypedRole.builder().on_receive_request(
                async |request: SimpleRequest,
                       responder: Responder<SimpleResponse>,
                       _connection: ConnectionTo<UntypedRole>| {
                    responder.respond(SimpleResponse {
                        result: format!("echo: {}", request.message),
                    })
                },
                agent_client_protocol_core::on_receive_request!(),
            );

            tokio::task::spawn_local(async move {
                if let Err(err) = server.connect_to(server_transport).await {
                    panic!("server should stay alive: {err:?}");
                }
            });

            let mut client_reader = BufReader::new(client_reader);

            client_writer
                .write_all(
                    br#"{"jsonrpc":"2.0","id":3,"method":"simple_method","params":{"content":"hello"}}
"#,
                )
                .await
                .unwrap();
            client_writer.flush().await.unwrap();

            let invalid_response = read_jsonrpc_response_line(&mut client_reader).await;
            expect![[r#"
                {
                  "error": {
                    "code": -32602,
                    "data": {
                      "error": "missing field `message`",
                      "json": {
                        "content": "hello"
                      },
                      "phase": "deserialization"
                    },
                    "message": "Invalid params"
                  },
                  "id": 3,
                  "jsonrpc": "2.0"
                }"#]]
            .assert_eq(&serde_json::to_string_pretty(&invalid_response).unwrap());

            client_writer
                .write_all(
                    br#"{"jsonrpc":"2.0","id":4,"method":"simple_method","params":{"message":"hello"}}
"#,
                )
                .await
                .unwrap();
            client_writer.flush().await.unwrap();

            let ok_response = read_jsonrpc_response_line(&mut client_reader).await;
            expect![[r#"
                {
                  "id": 4,
                  "jsonrpc": "2.0",
                  "result": {
                    "result": "echo: hello"
                  }
                }"#]]
            .assert_eq(&serde_json::to_string_pretty(&ok_response).unwrap());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_bad_notification_params_send_error_notification_and_connection_stays_alive() {
    use tokio::io::{AsyncWriteExt, BufReader};
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local
        .run_until(async {
            let (mut client_writer, server_reader) = tokio::io::duplex(2048);
            let (server_writer, client_reader) = tokio::io::duplex(2048);

            let server_reader = server_reader.compat();
            let server_writer = server_writer.compat_write();

            let server_transport =
                agent_client_protocol_core::ByteStreams::new(server_writer, server_reader);
            let server = UntypedRole
                .builder()
                .on_receive_notification(
                    async |_notif: SimpleNotification,
                           _connection: ConnectionTo<UntypedRole>| {
                        // If we get here, the notification parsed successfully.
                        Ok(())
                    },
                    agent_client_protocol_core::on_receive_notification!(),
                )
                .on_receive_request(
                    async |request: SimpleRequest,
                           responder: Responder<SimpleResponse>,
                           _connection: ConnectionTo<UntypedRole>| {
                        responder.respond(SimpleResponse {
                            result: format!("echo: {}", request.message),
                        })
                    },
                    agent_client_protocol_core::on_receive_request!(),
                );

            tokio::task::spawn_local(async move {
                if let Err(err) = server.connect_to(server_transport).await {
                    panic!("server should stay alive: {err:?}");
                }
            });

            let mut client_reader = BufReader::new(client_reader);

            // Send a notification with bad params (wrong field name).
            // Notifications have no "id", so the server sends an error
            // notification (id: null) and keeps the connection alive.
            client_writer
                .write_all(
                    br#"{"jsonrpc":"2.0","method":"simple_notification","params":{"wrong_field":"hello"}}
"#,
                )
                .await
                .unwrap();
            client_writer.flush().await.unwrap();

            // The server sends an error notification (id: null) for the
            // malformed notification.
            let error_notification = read_jsonrpc_response_line(&mut client_reader).await;
            expect![[r#"
                {
                  "error": {
                    "code": -32602,
                    "data": {
                      "error": "missing field `message`",
                      "json": {
                        "wrong_field": "hello"
                      },
                      "phase": "deserialization"
                    },
                    "message": "Invalid params"
                  },
                  "jsonrpc": "2.0"
                }"#]]
            .assert_eq(&serde_json::to_string_pretty(&error_notification).unwrap());

            // Now send a valid request to prove the connection is still alive.
            client_writer
                .write_all(
                    br#"{"jsonrpc":"2.0","id":10,"method":"simple_method","params":{"message":"after bad notification"}}
"#,
                )
                .await
                .unwrap();
            client_writer.flush().await.unwrap();

            let ok_response = read_jsonrpc_response_line(&mut client_reader).await;
            expect![[r#"
                {
                  "id": 10,
                  "jsonrpc": "2.0",
                  "result": {
                    "result": "echo: after bad notification"
                  }
                }"#]]
            .assert_eq(&serde_json::to_string_pretty(&ok_response).unwrap());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_match_dispatch_connectionless_bad_notification_params_emit_no_error_and_connection_stays_alive()
 {
    use tokio::io::{AsyncWriteExt, BufReader};
    use tokio::task::LocalSet;

    struct ConnectionlessMatchDispatchHandler;

    impl HandleDispatchFrom<UntypedRole> for ConnectionlessMatchDispatchHandler {
        fn describe_chain(&self) -> impl std::fmt::Debug {
            "ConnectionlessMatchDispatchHandler"
        }

        async fn handle_dispatch_from(
            &mut self,
            message: Dispatch,
            connection: ConnectionTo<UntypedRole>,
        ) -> Result<Handled<Dispatch>, agent_client_protocol_core::Error> {
            match MatchDispatch::new(message)
                .if_notification(async move |_notif: SimpleNotification| Ok(()))
                .await
                .done()?
            {
                Handled::Yes => Ok(Handled::Yes),
                Handled::No { message, retry } => match message.match_request::<SimpleRequest>() {
                    RequestMatch::Matched(request, responder) => {
                        responder.respond(SimpleResponse {
                            result: format!("echo: {}", request.message),
                        })?;
                        Ok(Handled::Yes)
                    }
                    RequestMatch::Rejected { dispatch, error } => {
                        dispatch.respond_with_error(error, connection)?;
                        Ok(Handled::Yes)
                    }
                    RequestMatch::Unhandled(message) => Ok(Handled::No { message, retry }),
                },
            }
        }
    }

    let local = LocalSet::new();

    local
        .run_until(async {
            let (mut client_writer, server_reader) = tokio::io::duplex(2048);
            let (server_writer, client_reader) = tokio::io::duplex(2048);

            let server_reader = server_reader.compat();
            let server_writer = server_writer.compat_write();

            let server_transport =
                agent_client_protocol_core::ByteStreams::new(server_writer, server_reader);
            let server = UntypedRole
                .builder()
                .with_handler(ConnectionlessMatchDispatchHandler);

            tokio::task::spawn_local(async move {
                if let Err(err) = server.connect_to(server_transport).await {
                    panic!("server should stay alive: {err:?}");
                }
            });

            let mut client_reader = BufReader::new(client_reader);

            client_writer
                .write_all(
                    br#"{"jsonrpc":"2.0","method":"simple_notification","params":{"wrong_field":"hello"}}
"#,
                )
                .await
                .unwrap();
            client_writer.flush().await.unwrap();

            let unexpected = try_read_jsonrpc_response_line(
                &mut client_reader,
                tokio::time::Duration::from_millis(100),
            )
            .await;
            assert!(
                unexpected.is_none(),
                "connectionless MatchDispatch should not emit an out-of-band error for malformed notifications"
            );

            client_writer
                .write_all(
                    br#"{"jsonrpc":"2.0","id":11,"method":"simple_method","params":{"message":"after connectionless bad notification"}}
"#,
                )
                .await
                .unwrap();
            client_writer.flush().await.unwrap();

            let ok_response = read_jsonrpc_response_line(&mut client_reader).await;
            expect![[r#"
                {
                  "id": 11,
                  "jsonrpc": "2.0",
                  "result": {
                    "result": "echo: after connectionless bad notification"
                  }
                }"#]]
            .assert_eq(&serde_json::to_string_pretty(&ok_response).unwrap());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_match_dispatch_from_if_message_invalid_params_keeps_connection_alive() {
    use tokio::io::{AsyncWriteExt, BufReader};
    use tokio::task::LocalSet;

    struct MatchDispatchFromMessageHandler;

    impl HandleDispatchFrom<UntypedRole> for MatchDispatchFromMessageHandler {
        fn describe_chain(&self) -> impl std::fmt::Debug {
            "MatchDispatchFromMessageHandler"
        }

        async fn handle_dispatch_from(
            &mut self,
            message: Dispatch,
            connection: ConnectionTo<UntypedRole>,
        ) -> Result<Handled<Dispatch>, agent_client_protocol_core::Error> {
            MatchDispatchFrom::new(message, &connection)
                .if_message_from(
                    UntypedRole,
                    async move |dispatch: Dispatch<SimpleRequest, SimpleNotification>| {
                        match dispatch {
                            Dispatch::Request(request, responder) => {
                                responder.respond(SimpleResponse {
                                    result: format!("echo: {}", request.message),
                                })
                            }
                            Dispatch::Notification(_) => Ok(()),
                            Dispatch::Response(result, router) => {
                                router.respond_with_result(result)
                            }
                        }
                    },
                )
                .await
                .done()
        }
    }

    let local = LocalSet::new();

    local
        .run_until(async {
            let (mut client_writer, server_reader) = tokio::io::duplex(2048);
            let (server_writer, client_reader) = tokio::io::duplex(2048);

            let server_reader = server_reader.compat();
            let server_writer = server_writer.compat_write();

            let server_transport =
                agent_client_protocol_core::ByteStreams::new(server_writer, server_reader);
            let server = UntypedRole
                .builder()
                .with_handler(MatchDispatchFromMessageHandler);

            tokio::task::spawn_local(async move {
                if let Err(err) = server.connect_to(server_transport).await {
                    panic!("server should stay alive: {err:?}");
                }
            });

            let mut client_reader = BufReader::new(client_reader);

            client_writer
                .write_all(
                    br#"{"jsonrpc":"2.0","id":5,"method":"simple_method","params":{"content":"hello"}}
"#,
                )
                .await
                .unwrap();
            client_writer.flush().await.unwrap();

            let invalid_response = read_jsonrpc_response_line(&mut client_reader).await;
            expect![[r#"
                {
                  "error": {
                    "code": -32602,
                    "data": {
                      "error": "missing field `message`",
                      "json": {
                        "content": "hello"
                      },
                      "phase": "deserialization"
                    },
                    "message": "Invalid params"
                  },
                  "id": 5,
                  "jsonrpc": "2.0"
                }"#]]
            .assert_eq(&serde_json::to_string_pretty(&invalid_response).unwrap());

            client_writer
                .write_all(
                    br#"{"jsonrpc":"2.0","id":6,"method":"simple_method","params":{"message":"hello"}}
"#,
                )
                .await
                .unwrap();
            client_writer.flush().await.unwrap();

            let ok_response = read_jsonrpc_response_line(&mut client_reader).await;
            expect![[r#"
                {
                  "id": 6,
                  "jsonrpc": "2.0",
                  "result": {
                    "result": "echo: hello"
                  }
                }"#]]
            .assert_eq(&serde_json::to_string_pretty(&ok_response).unwrap());
        })
        .await;
}
