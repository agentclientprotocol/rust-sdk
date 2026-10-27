//! Capture ordinary warning output, not just the returned protocol error.

use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use agent_client_protocol::{
    ConnectionTo, Error, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    Responder, UntypedMessage, role::UntypedRole, util::TypeNotification,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const PRIVATE_MARKER: &str = "SDK_2_SYNTHETIC_PRIVATE_TERMINAL_OUTPUT";

#[derive(Clone)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl LogCapture {
    fn new() -> Self {
        Self(Arc::default())
    }

    fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync + 'static {
        self.subscriber_at(tracing::Level::WARN)
    }

    fn subscriber_at(
        &self,
        level: tracing::Level,
    ) -> impl tracing::Subscriber + Send + Sync + 'static {
        let writer = self.clone();
        tracing_subscriber::fmt()
            .with_max_level(level)
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish()
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Kind {
    Known,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonRpcNotification)]
#[notification(method = "_test/private_notification")]
struct Notice {
    required: u64,
    kind: Kind,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_test/private_request", response = Response)]
struct Request {
    required: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonRpcResponse)]
struct Response {}

#[test]
fn type_notification_warnings_do_not_echo_private_params_or_parse_reasons() {
    let capture = LogCapture::new();
    tracing::subscriber::with_default(capture.subscriber(), || {
        futures::executor::block_on(async {
            for params in [
                json!({ "extra": PRIVATE_MARKER }),
                json!({ "required": PRIVATE_MARKER, "kind": "Known" }),
                json!({ "required": 1, "kind": PRIVATE_MARKER }),
            ] {
                // The full error remains available to callers; neither its JSON
                // nor a Serde reason quoting an invalid value is safe for WARN.
                let error =
                    Notice::parse_message("_test/private_notification", &params).unwrap_err();
                assert!(format!("{error:?}").contains(PRIVATE_MARKER));

                TypeNotification::new(
                    UntypedMessage::new("_test/private_notification", params).unwrap(),
                )
                .handle_if(async |_: Notice| {
                    panic!("malformed notification must not reach its handler");
                })
                .await
                .otherwise(async |_| {
                    panic!("malformed matching notification must not reach fallback");
                })
                .await
                .unwrap();
            }
        });
    });

    let logs = capture.text();
    assert_eq!(logs.matches("Invalid notification params;").count(), 3);
    assert!(logs.contains("_test/private_notification"));
    assert!(logs.contains("-32602"));
    assert!(logs.contains("Invalid params"));
    assert!(!logs.contains(PRIVATE_MARKER), "{logs}");
}

fn private_handler_error() -> Error {
    // Both fields are application-controlled, even for a standard error code.
    Error::new(-32602, PRIVATE_MARKER).data(json!({ "private": PRIVATE_MARKER }))
}

#[tokio::test(flavor = "current_thread")]
async fn dispatch_and_error_response_warnings_do_not_echo_private_errors() {
    let capture = LogCapture::new();
    let _subscriber = tracing::subscriber::set_default(capture.subscriber());
    tokio::task::LocalSet::new()
        .run_until(async {
            let (mut client_writer, server_reader) = tokio::io::duplex(4096);
            let (server_writer, client_reader) = tokio::io::duplex(4096);
            let server = UntypedRole
                .builder()
                .on_receive_notification(
                    async |_: Notice, _: ConnectionTo<UntypedRole>| {
                        Err::<(), _>(private_handler_error())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .on_receive_request(
                    async |request: Request,
                           responder: Responder<Response>,
                           _: ConnectionTo<UntypedRole>| {
                        if request.required == 0 {
                            responder.respond(Response {})
                        } else {
                            Err(private_handler_error())
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                );
            let task = tokio::task::spawn_local(async move {
                server
                    .connect_to(agent_client_protocol::ByteStreams::new(
                        server_writer.compat_write(),
                        server_reader.compat(),
                    ))
                    .await
                    .unwrap();
            });
            let mut client_reader = BufReader::new(client_reader);

            for params in [
                json!({ "required": PRIVATE_MARKER, "kind": "Known" }),
                json!({ "required": 1, "kind": "Known" }),
            ] {
                let line = json!({
                    "jsonrpc": "2.0",
                    "method": "_test/private_notification",
                    "params": params,
                });
                client_writer
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .unwrap();
            }
            for (id, params) in [
                (1, json!({ "required": PRIVATE_MARKER })),
                (2, json!({ "required": 1 })),
                (3, json!({ "required": 0 })),
            ] {
                let line = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "_test/private_request",
                    "params": params,
                });
                client_writer
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .unwrap();
                client_writer.flush().await.unwrap();

                let mut response = String::new();
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    client_reader.read_line(&mut response),
                )
                .await
                .expect("server must respond and remain alive")
                .unwrap();
                let response: Value = serde_json::from_str(&response).unwrap();
                // No notification replies may precede the request response.
                assert_eq!(response["id"], id);
                if id < 3 {
                    assert_eq!(response["error"]["code"], -32602);
                    assert!(response.to_string().contains(PRIVATE_MARKER));
                } else {
                    assert_eq!(response["result"], json!({}));
                }
            }
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        })
        .await;

    let logs = capture.text();
    // Assert that real warnings were captured from both actors, not an empty log.
    assert!(logs.contains("Handler errored"), "{logs}");
    assert!(logs.contains("Ignoring message-processing error"), "{logs}");
    assert_eq!(logs.matches("Sending error response").count(), 2, "{logs}");
    assert!(logs.contains("_test/private_notification"));
    assert!(logs.contains("_test/private_request"));
    assert!(logs.contains("-32602"));
    assert!(!logs.contains(PRIVATE_MARKER), "{logs}");
}

const SECRET: &str = "SDK_SYNTHETIC_PRIVATE_CREDENTIAL";

/// Exchange a `session/new` carrying MCP `env`/`headers` secrets and a response
/// carrying terminal auth `env` over a real byte stream, capturing all logs at
/// `level`.
async fn exchange_secret_bearing_messages(level: tracing::Level) -> String {
    use agent_client_protocol::schema::v1::{
        EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerStdio, NewSessionRequest,
        NewSessionResponse,
    };

    let capture = LogCapture::new();
    let _subscriber = tracing::subscriber::set_default(capture.subscriber_at(level));
    tokio::task::LocalSet::new()
        .run_until(async {
            let (client_writer, server_reader) = tokio::io::duplex(4096);
            let (server_writer, client_reader) = tokio::io::duplex(4096);
            let server = UntypedRole
                .builder()
                .on_receive_request(
                    async |request: NewSessionRequest,
                           responder: Responder<NewSessionResponse>,
                           _: ConnectionTo<UntypedRole>| {
                        assert_eq!(request.mcp_servers.len(), 2);
                        responder.respond(NewSessionResponse::new("session-1"))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async |_: UntypedMessage,
                           responder: Responder<Value>,
                           _: ConnectionTo<UntypedRole>| {
                        responder.respond(json!({ "authMethods": [{
                            "type": "terminal", "id": "login", "name": "Login",
                            "env": { "LOGIN_TOKEN": SECRET },
                        }]}))
                    },
                    agent_client_protocol::on_receive_request!(),
                );
            tokio::task::spawn_local(server.connect_to(agent_client_protocol::ByteStreams::new(
                server_writer.compat_write(),
                server_reader.compat(),
            )));

            UntypedRole
                .builder()
                .connect_with(
                    agent_client_protocol::ByteStreams::new(
                        client_writer.compat_write(),
                        client_reader.compat(),
                    ),
                    async |cx| {
                        let request = NewSessionRequest::new("/work").mcp_servers(vec![
                            McpServer::Stdio(
                                McpServerStdio::new("stdio", "/bin/server")
                                    .env(vec![EnvVariable::new("API_KEY", SECRET)]),
                            ),
                            McpServer::Http(
                                McpServerHttp::new("http", "https://example.test/mcp")
                                    .headers(vec![HttpHeader::new("X-Api-Token", SECRET)]),
                            ),
                        ]);
                        let response = cx.send_request(request).block_task().await?;
                        assert_eq!(response.session_id.to_string(), "session-1");
                        let response = cx
                            .send_request(UntypedMessage::new("_test/auth", json!({}))?)
                            .block_task()
                            .await?;
                        assert_eq!(response["authMethods"][0]["env"]["LOGIN_TOKEN"], SECRET);
                        Ok(())
                    },
                )
                .await
                .unwrap();
        })
        .await;
    capture.text()
}

#[tokio::test(flavor = "current_thread")]
async fn debug_logs_omit_session_setup_payloads() {
    let logs = exchange_secret_bearing_messages(tracing::Level::DEBUG).await;
    // Real DEBUG events were captured, identifying the method only.
    assert!(logs.contains("session/new"), "{logs}");
    assert!(!logs.contains(SECRET), "{logs}");
    assert!(!logs.contains("API_KEY"), "{logs}");
    assert!(!logs.contains("X-Api-Token"), "{logs}");
}

#[tokio::test(flavor = "current_thread")]
async fn trace_logs_redact_env_and_header_values() {
    let logs = exchange_secret_bearing_messages(tracing::Level::TRACE).await;
    // Wire-level TRACE events record the messages, keeping names visible.
    assert!(logs.contains("Sending JSON-RPC message"), "{logs}");
    assert!(logs.contains("Received JSON-RPC message"), "{logs}");
    for name in ["API_KEY", "X-Api-Token", "LOGIN_TOKEN"] {
        assert!(logs.contains(name), "{name} missing from {logs}");
    }
    assert!(logs.contains("[REDACTED]"), "{logs}");
    assert!(!logs.contains(SECRET), "{logs}");
}
