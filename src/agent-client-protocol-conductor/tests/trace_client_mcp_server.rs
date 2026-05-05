//! Snapshot test for trace events when a client-hosted MCP server handles tool calls.
//!
//! This test demonstrates the full round-trip flow:
//! 1. Client → Agent: initialize, session/new, session/prompt (left-to-right ACP)
//! 2. Agent → Client: MCP initialize, tools/call (right-to-left MCP, all the way back!)
//! 3. Client → Agent: MCP response
//! 4. Agent → Client: session/update notification, prompt response
//!
//! Unlike trace_mcp_tool_call.rs which tests proxy-hosted MCP servers, this test
//! verifies that MCP requests travel all the way back to the client.

use agent_client_protocol::mcp_server::McpServer;
use agent_client_protocol::schema::{InitializeRequest, ProtocolVersion};
use agent_client_protocol::{Client, Role, RunWithConnectionTo};
use agent_client_protocol_conductor::trace::TraceEvent;
use agent_client_protocol_conductor::{ConductorImpl, McpBridgeMode, ProxiesAndAgent};
use agent_client_protocol_test::testy::{Testy, TestyCommand};
use expect_test::expect;
use futures::StreamExt;
use futures::channel::mpsc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
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

/// Create an MCP server with an echo tool for testing.
fn make_echo_mcp_server<R: Role>(
    call_count: &Mutex<usize>,
) -> McpServer<R::Counterpart, impl RunWithConnectionTo<R::Counterpart>> {
    #[derive(Serialize, Deserialize, JsonSchema)]
    struct EchoInput {
        message: String,
    }

    #[derive(Serialize, JsonSchema)]
    struct EchoOutput {
        echoed: String,
        call_number: usize,
    }

    McpServer::builder("echo-server".to_string())
        .instructions("A test MCP server hosted by the client")
        .tool_fn_mut(
            "echo",
            "Echoes back the input message",
            async |input: EchoInput, _cx| {
                let mut count = call_count.lock().expect("not poisoned");
                *count += 1;
                Ok(EchoOutput {
                    echoed: format!("Client echoes: {}", input.message),
                    call_number: *count,
                })
            },
            agent_client_protocol::tool_fn_mut!(),
        )
        .build()
}

#[tokio::test]
async fn test_trace_client_mcp_server() -> Result<(), agent_client_protocol::Error> {
    // Create channel for collecting trace events
    let (trace_tx, trace_rx) = mpsc::unbounded();

    // Create duplex streams for client <-> conductor communication
    let (client_write, conductor_read) = duplex(8192);
    let (conductor_write, client_read) = duplex(8192);

    // Spawn the conductor with ElizaAgent (no proxies - simple setup)
    let conductor_handle = tokio::spawn(async move {
        ConductorImpl::new_agent(
            "conductor".to_string(),
            ProxiesAndAgent::new(Testy::new()),
            McpBridgeMode::default(),
        )
        .trace_to(trace_tx)
        .run(agent_client_protocol::ByteStreams::new(
            conductor_write.compat_write(),
            conductor_read.compat(),
        ))
        .await
    });

    // Run the client with a client-hosted MCP server
    let test_result = tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        agent_client_protocol::Client
            .builder()
            .name("test-client")
            .connect_with(
                agent_client_protocol::ByteStreams::new(
                    client_write.compat_write(),
                    client_read.compat(),
                ),
                async |cx| {
                    // Initialize
                    cx.send_request(InitializeRequest::new(ProtocolVersion::LATEST))
                        .block_task()
                        .await?;

                    // Stack-local state that the MCP tool will modify
                    let call_count = Mutex::new(0usize);

                    // Build session with client-hosted MCP server
                    let result = cx
                        .build_session(".")
                        .with_mcp_server(make_echo_mcp_server::<Client>(&call_count))?
                        .block_task()
                        .run_until(async |mut session| {
                            // Send prompt that triggers MCP tool call
                            // The tool call will travel: agent → conductor → client
                            session.send_prompt(TestyCommand::CallTool {
                                server: "echo-server".to_string(),
                                tool: "echo".to_string(),
                                params: serde_json::json!({"message": "Hello from client test!"}),
                            }.to_prompt())?;
                            session.read_to_string().await
                        })
                        .await?;

                    // Verify the tool was called
                    assert_eq!(*call_count.lock().unwrap(), 1);

                    // Verify the response contains our echo
                    assert!(result.contains("Client echoes: Hello from client test!"));

                    Ok(())
                },
            )
            .await
    })
    .await
    .expect("Test timed out");

    // Abort the conductor to close the trace channel
    conductor_handle.abort();

    // Collect and normalize trace events
    let mut normalizer = EventNormalizer::new();
    let events = normalizer.normalize_events(trace_rx.collect().await);

    // Snapshot the trace events
    // This should show the full round-trip:
    // 1. Client -> Agent: initialize, session/new, session/prompt (left-to-right ACP)
    // 2. Agent -> Client: MCP initialize, tools/call (right-to-left MCP - all the way back!)
    // 3. Client -> Agent: MCP response
    // 4. Agent -> Client: session/update notification, prompt response
    expect![[r#"
        [
            Request(
                RequestEvent {
                    ts: 0.0,
                    protocol: Acp,
                    from: "Client",
                    to: "Agent",
                    id: String("id:0"),
                    method: "initialize",
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
                    from: "Agent",
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
                    to: "Agent",
                    id: String("id:1"),
                    method: "session/new",
                    session: None,
                    params: Object {
                        "cwd": String("."),
                        "mcpServers": Array [
                            Object {
                                "headers": Array [],
                                "name": String("echo-server"),
                                "type": String("http"),
                                "url": String("acp:url:0"),
                            },
                        ],
                    },
                },
            ),
            Request(
                RequestEvent {
                    ts: 0.0,
                    protocol: Acp,
                    from: "Agent",
                    to: "Client",
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
                    from: "Agent",
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
                    to: "Agent",
                    id: String("id:3"),
                    method: "session/prompt",
                    session: None,
                    params: Object {
                        "prompt": Array [
                            Object {
                                "text": String("{\"command\":\"call_tool\",\"server\":\"echo-server\",\"tool\":\"echo\",\"params\":{\"message\":\"Hello from client test!\"}}"),
                                "type": String("text"),
                            },
                        ],
                        "sessionId": String("session:0"),
                    },
                },
            ),
            Response(
                ResponseEvent {
                    ts: 0.0,
                    from: "Client",
                    to: "Agent",
                    id: String("id:2"),
                    is_error: false,
                    payload: Object {
                        "connection_id": String("connection:0"),
                    },
                },
            ),
            Request(
                RequestEvent {
                    ts: 0.0,
                    protocol: Mcp,
                    from: "Agent",
                    to: "Client",
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
                    from: "Client",
                    to: "Agent",
                    id: String("id:4"),
                    is_error: false,
                    payload: Object {
                        "capabilities": Object {
                            "tools": Object {},
                        },
                        "instructions": String("A test MCP server hosted by the client"),
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
                    from: "Agent",
                    to: "Client",
                    method: "notifications/initialized",
                    session: None,
                    params: Null,
                },
            ),
            Request(
                RequestEvent {
                    ts: 0.0,
                    protocol: Mcp,
                    from: "Agent",
                    to: "Client",
                    id: String("id:5"),
                    method: "tools/call",
                    session: None,
                    params: Object {
                        "_meta": Object {
                            "progressToken": Number(0),
                        },
                        "arguments": Object {
                            "message": String("Hello from client test!"),
                        },
                        "name": String("echo"),
                    },
                },
            ),
            Response(
                ResponseEvent {
                    ts: 0.0,
                    from: "Client",
                    to: "Agent",
                    id: String("id:5"),
                    is_error: false,
                    payload: Object {
                        "content": Array [
                            Object {
                                "text": String("{\"call_number\":1,\"echoed\":\"Client echoes: Hello from client test!\"}"),
                                "type": String("text"),
                            },
                        ],
                        "isError": Bool(false),
                        "structuredContent": Object {
                            "call_number": Number(1),
                            "echoed": String("Client echoes: Hello from client test!"),
                        },
                    },
                },
            ),
            Notification(
                NotificationEvent {
                    ts: 0.0,
                    protocol: Acp,
                    from: "Agent",
                    to: "Client",
                    method: "session/update",
                    session: None,
                    params: Object {
                        "sessionId": String("session:0"),
                        "update": Object {
                            "content": Object {
                                "text": String("OK: CallToolResult { content: [Annotated { raw: Text(RawTextContent { text: \"{\\\"call_number\\\":1,\\\"echoed\\\":\\\"Client echoes: Hello from client test!\\\"}\", meta: None }), annotations: None }], structured_content: Some(Object {\"call_number\": Number(1), \"echoed\": String(\"Client echoes: Hello from client test!\")}), is_error: Some(false), meta: None }"),
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
                    from: "Agent",
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
