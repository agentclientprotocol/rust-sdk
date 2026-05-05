//! Snapshot test for trace events when an agent makes an MCP tool call.
//!
//! This test verifies the right-to-left request flow:
//! - Client sends prompt to agent
//! - Agent makes MCP tools/call request back through the conductor
//! - Conductor routes the request to the proxy's MCP server
//! - Response flows back to the agent
//!
//! This captures trace events for the full bidirectional flow.

mod mcp_integration;

use agent_client_protocol::schema::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, ProtocolVersion,
    SessionNotification, TextContent,
};
use agent_client_protocol_conductor::trace::TraceEvent;
use agent_client_protocol_conductor::{ConductorImpl, McpBridgeMode, ProxiesAndAgent};
use agent_client_protocol_test::testy::{Testy, TestyCommand};
use expect_test::expect;
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Normalize events for stable snapshot testing.
///
/// - Strips timestamps (set to 0.0)
/// - Replaces UUIDs with sequential IDs (id:0, id:1, etc.)
/// - Replaces session IDs with "session:0", etc.
/// - Replaces acp: URLs with "acp:url:0", etc.
/// - Replaces connection_id with "connection:0", etc.
struct EventNormalizer {
    id_map: HashMap<String, String>,
    next_id: usize,
    session_map: HashMap<String, String>,
    next_session: usize,
    acp_url_map: HashMap<String, String>,
    next_acp_url: usize,
    connection_map: HashMap<String, String>,
    next_connection: usize,
}

impl EventNormalizer {
    fn new() -> Self {
        Self {
            id_map: HashMap::new(),
            next_id: 0,
            session_map: HashMap::new(),
            next_session: 0,
            acp_url_map: HashMap::new(),
            next_acp_url: 0,
            connection_map: HashMap::new(),
            next_connection: 0,
        }
    }

    fn normalize_id(&mut self, id: serde_json::Value) -> serde_json::Value {
        let id_str = match &id {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => return id,
        };

        let normalized = self.id_map.entry(id_str).or_insert_with(|| {
            let n = format!("id:{}", self.next_id);
            self.next_id += 1;
            n
        });

        serde_json::Value::String(normalized.clone())
    }

    fn normalize_session(&mut self, session: Option<String>) -> Option<String> {
        session.map(|s| self.normalize_session_id(&s))
    }

    fn normalize_session_id(&mut self, session: &str) -> String {
        self.session_map
            .entry(session.to_string())
            .or_insert_with(|| {
                let n = format!("session:{}", self.next_session);
                self.next_session += 1;
                n
            })
            .clone()
    }

    fn normalize_acp_url(&mut self, url: &str) -> String {
        self.acp_url_map
            .entry(url.to_string())
            .or_insert_with(|| {
                let n = format!("acp:url:{}", self.next_acp_url);
                self.next_acp_url += 1;
                n
            })
            .clone()
    }

    fn normalize_connection_id(&mut self, id: &str) -> String {
        self.connection_map
            .entry(id.to_string())
            .or_insert_with(|| {
                let n = format!("connection:{}", self.next_connection);
                self.next_connection += 1;
                n
            })
            .clone()
    }

    /// Recursively normalize session IDs, acp: URLs, and connection IDs in JSON values.
    fn normalize_json(&mut self, value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let normalized: serde_json::Map<String, serde_json::Value> = map
                    .into_iter()
                    .map(|(k, v)| {
                        let v = if k == "sessionId" {
                            if let serde_json::Value::String(s) = &v {
                                serde_json::Value::String(self.normalize_session_id(s))
                            } else {
                                self.normalize_json(v)
                            }
                        } else if k == "url" || k == "acp_url" {
                            if let serde_json::Value::String(s) = &v {
                                if s.starts_with("acp:") || s.starts_with("http://localhost:") {
                                    serde_json::Value::String(self.normalize_acp_url(s))
                                } else {
                                    v
                                }
                            } else {
                                self.normalize_json(v)
                            }
                        } else if k == "connection_id" {
                            if let serde_json::Value::String(s) = &v {
                                serde_json::Value::String(self.normalize_connection_id(s))
                            } else {
                                self.normalize_json(v)
                            }
                        } else {
                            self.normalize_json(v)
                        };
                        (k, v)
                    })
                    .collect();
                serde_json::Value::Object(normalized)
            }
            serde_json::Value::Array(arr) => {
                serde_json::Value::Array(arr.into_iter().map(|v| self.normalize_json(v)).collect())
            }
            other => other,
        }
    }

    fn normalize_events(&mut self, events: Vec<TraceEvent>) -> Vec<TraceEvent> {
        events
            .into_iter()
            .map(|event| match event {
                TraceEvent::Request(mut r) => {
                    r.ts = 0.0;
                    r.id = self.normalize_id(r.id);
                    r.session = self.normalize_session(r.session);
                    r.params = self.normalize_json(r.params);
                    TraceEvent::Request(r)
                }
                TraceEvent::Response(mut r) => {
                    r.ts = 0.0;
                    r.id = self.normalize_id(r.id);
                    r.payload = self.normalize_json(r.payload);
                    TraceEvent::Response(r)
                }
                TraceEvent::Notification(mut n) => {
                    n.ts = 0.0;
                    n.session = self.normalize_session(n.session);
                    n.params = self.normalize_json(n.params);
                    TraceEvent::Notification(n)
                }
                _ => panic!("unknown trace event type"),
            })
            .collect()
    }
}

/// Test helper to receive a JSON-RPC response
async fn recv<T: agent_client_protocol::JsonRpcResponse + Send>(
    response: agent_client_protocol::SentRequest<T>,
) -> Result<T, agent_client_protocol::Error> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    response.on_receiving_result(async move |result| {
        tx.send(result)
            .map_err(|_| agent_client_protocol::Error::internal_error())
    })?;
    rx.await
        .map_err(|_| agent_client_protocol::Error::internal_error())?
}

#[tokio::test]
async fn test_trace_mcp_tool_call() -> Result<(), agent_client_protocol::Error> {
    // Create channel for collecting trace events
    let (trace_tx, trace_rx) = mpsc::unbounded();

    // Create channel to collect notifications (to verify test worked)
    let (notif_tx, mut notif_rx) = mpsc::unbounded();

    // Create duplex streams for client <-> conductor communication
    let (client_write, conductor_read) = duplex(8192);
    let (conductor_write, client_read) = duplex(8192);

    // Spawn the conductor with:
    // - ElizaAgent (deterministic mode) as the agent
    // - ProxyComponent that provides the "test" MCP server with echo tool
    // - Tracing enabled to capture events
    let conductor_handle = tokio::spawn(async move {
        ConductorImpl::new_agent(
            "conductor".to_string(),
            ProxiesAndAgent::new(Testy::new()).proxy(mcp_integration::proxy::ProxyComponent),
            McpBridgeMode::default(),
        )
        .trace_to(trace_tx)
        .run(agent_client_protocol::ByteStreams::new(
            conductor_write.compat_write(),
            conductor_read.compat(),
        ))
        .await
    });

    // Run the client interaction
    let test_result = tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        agent_client_protocol::Client
            .builder()
            .name("test-client")
            .on_receive_notification(
                {
                    let mut notif_tx = notif_tx;
                    async move |notification: SessionNotification, _cx| {
                        notif_tx
                            .send(notification)
                            .await
                            .map_err(|_| agent_client_protocol::Error::internal_error())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                agent_client_protocol::ByteStreams::new(
                    client_write.compat_write(),
                    client_read.compat(),
                ),
                async |cx| {
                    // Initialize
                    recv(cx.send_request(InitializeRequest::new(ProtocolVersion::LATEST))).await?;

                    // Create session
                    let session = recv(
                        cx.send_request(NewSessionRequest::new(std::path::PathBuf::from("/"))),
                    )
                    .await?;

                    // Send prompt that triggers MCP tool call
                    recv(cx.send_request(PromptRequest::new(
                        session.session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new(TestyCommand::CallTool {
                            server: "test".to_string(),
                            tool: "echo".to_string(),
                            params: serde_json::json!({"message": "Hello from trace test!"}),
                        }.to_prompt()))],
                    )))
                    .await?;

                    Ok(())
                },
            )
            .await
    })
    .await
    .expect("Test timed out");

    // Abort the conductor to close the trace channel
    conductor_handle.abort();
    let mut notifications = Vec::new();
    while let Some(notif) = notif_rx.next().await {
        notifications.push(notif);
    }
    assert_eq!(notifications.len(), 1, "Expected one notification");

    // Collect and normalize trace events
    let mut normalizer = EventNormalizer::new();
    let events = normalizer.normalize_events(trace_rx.collect().await);

    // Snapshot the trace events
    // This should show:
    // 1. Client -> Agent: initialize, session/new, session/prompt (left-to-right)
    // 2. Agent -> MCP Server: tools/call (right-to-left, the key part!)
    // 3. MCP Server -> Agent: response
    // 4. Agent -> Client: notification + response
    expect![[r#"
        [
            Request(
                RequestEvent {
                    ts: 0.0,
                    protocol: Acp,
                    from: "Client",
                    to: "Proxy(0)",
                    id: String("id:0"),
                    method: "_proxy/initialize",
                    session: None,
                    params: Object {
                        "clientCapabilities": Object {
                            "auth": Object {
                                "terminal": Bool(false),
                            },
                            "fs": Object {
                                "readTextFile": Bool(false),
                                "writeTextFile": Bool(false),
                            },
                            "terminal": Bool(false),
                        },
                        "protocolVersion": Number(1),
                    },
                },
            ),
            Response(
                ResponseEvent {
                    ts: 0.0,
                    from: "Proxy(0)",
                    to: "Client",
                    id: String("id:0"),
                    is_error: false,
                    payload: Object {
                        "agentCapabilities": Object {
                            "auth": Object {},
                            "loadSession": Bool(false),
                            "mcpCapabilities": Object {
                                "http": Bool(false),
                                "sse": Bool(false),
                            },
                            "promptCapabilities": Object {
                                "audio": Bool(false),
                                "embeddedContext": Bool(false),
                                "image": Bool(false),
                            },
                            "sessionCapabilities": Object {},
                        },
                        "authMethods": Array [],
                        "protocolVersion": Number(1),
                    },
                },
            ),
            Request(
                RequestEvent {
                    ts: 0.0,
                    protocol: Acp,
                    from: "Client",
                    to: "Proxy(0)",
                    id: String("id:1"),
                    method: "session/new",
                    session: None,
                    params: Object {
                        "cwd": String("/"),
                        "mcpServers": Array [],
                    },
                },
            ),
            Request(
                RequestEvent {
                    ts: 0.0,
                    protocol: Acp,
                    from: "Proxy(1)",
                    to: "Proxy(0)",
                    id: String("id:2"),
                    method: "_mcp/connect",
                    session: None,
                    params: Object {
                        "acp_url": String("acp:url:0"),
                    },
                },
            ),
            Response(
                ResponseEvent {
                    ts: 0.0,
                    from: "Proxy(0)",
                    to: "Proxy(1)",
                    id: String("id:2"),
                    is_error: false,
                    payload: Object {
                        "connection_id": String("connection:0"),
                    },
                },
            ),
            Response(
                ResponseEvent {
                    ts: 0.0,
                    from: "Proxy(0)",
                    to: "Client",
                    id: String("id:1"),
                    is_error: false,
                    payload: Object {
                        "sessionId": String("session:0"),
                    },
                },
            ),
            Request(
                RequestEvent {
                    ts: 0.0,
                    protocol: Acp,
                    from: "Client",
                    to: "Proxy(0)",
                    id: String("id:3"),
                    method: "session/prompt",
                    session: None,
                    params: Object {
                        "prompt": Array [
                            Object {
                                "text": String("{\"command\":\"call_tool\",\"server\":\"test\",\"tool\":\"echo\",\"params\":{\"message\":\"Hello from trace test!\"}}"),
                                "type": String("text"),
                            },
                        ],
                        "sessionId": String("session:0"),
                    },
                },
            ),
            Request(
                RequestEvent {
                    ts: 0.0,
                    protocol: Mcp,
                    from: "Proxy(1)",
                    to: "Proxy(0)",
                    id: String("id:4"),
                    method: "initialize",
                    session: None,
                    params: Object {
                        "capabilities": Object {},
                        "clientInfo": Object {
                            "name": String("rmcp"),
                            "version": String("1.6.0"),
                        },
                        "protocolVersion": String("2025-11-25"),
                    },
                },
            ),
            Response(
                ResponseEvent {
                    ts: 0.0,
                    from: "Proxy(0)",
                    to: "Proxy(1)",
                    id: String("id:4"),
                    is_error: false,
                    payload: Object {
                        "capabilities": Object {
                            "tools": Object {},
                        },
                        "instructions": String("A simple test MCP server with an echo tool"),
                        "protocolVersion": String("2025-11-25"),
                        "serverInfo": Object {
                            "name": String("rmcp"),
                            "version": String("1.6.0"),
                        },
                    },
                },
            ),
            Notification(
                NotificationEvent {
                    ts: 0.0,
                    protocol: Mcp,
                    from: "Proxy(1)",
                    to: "Proxy(0)",
                    method: "notifications/initialized",
                    session: None,
                    params: Null,
                },
            ),
            Request(
                RequestEvent {
                    ts: 0.0,
                    protocol: Mcp,
                    from: "Proxy(1)",
                    to: "Proxy(0)",
                    id: String("id:5"),
                    method: "tools/call",
                    session: None,
                    params: Object {
                        "_meta": Object {
                            "progressToken": Number(0),
                        },
                        "arguments": Object {
                            "message": String("Hello from trace test!"),
                        },
                        "name": String("echo"),
                    },
                },
            ),
            Response(
                ResponseEvent {
                    ts: 0.0,
                    from: "Proxy(0)",
                    to: "Proxy(1)",
                    id: String("id:5"),
                    is_error: false,
                    payload: Object {
                        "content": Array [
                            Object {
                                "text": String("{\"result\":\"Echo: Hello from trace test!\"}"),
                                "type": String("text"),
                            },
                        ],
                        "isError": Bool(false),
                        "structuredContent": Object {
                            "result": String("Echo: Hello from trace test!"),
                        },
                    },
                },
            ),
            Notification(
                NotificationEvent {
                    ts: 0.0,
                    protocol: Acp,
                    from: "Proxy(1)",
                    to: "Proxy(0)",
                    method: "session/update",
                    session: None,
                    params: Object {
                        "sessionId": String("session:0"),
                        "update": Object {
                            "content": Object {
                                "text": String("OK: CallToolResult { content: [Annotated { raw: Text(RawTextContent { text: \"{\\\"result\\\":\\\"Echo: Hello from trace test!\\\"}\", meta: None }), annotations: None }], structured_content: Some(Object {\"result\": String(\"Echo: Hello from trace test!\")}), is_error: Some(false), meta: None }"),
                                "type": String("text"),
                            },
                            "sessionUpdate": String("agent_message_chunk"),
                        },
                    },
                },
            ),
            Response(
                ResponseEvent {
                    ts: 0.0,
                    from: "Proxy(0)",
                    to: "Client",
                    id: String("id:3"),
                    is_error: false,
                    payload: Object {
                        "stopReason": String("end_turn"),
                    },
                },
            ),
        ]
    "#]]
    .assert_debug_eq(&events);

    test_result?;

    Ok(())
}
