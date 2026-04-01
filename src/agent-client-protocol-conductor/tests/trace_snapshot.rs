//! Snapshot test for trace events from a real yopo interaction.
//!
//! This test runs yopo -> conductor (with arrow_proxy -> test_agent) and
//! captures trace events to a channel for expect_test snapshot verification.
//!
//! Run `just prep-tests` before running this test.

use agent_client_protocol_conductor::trace::TraceEvent;
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use agent_client_protocol_test::test_binaries::{arrow_proxy_example, testy};
use agent_client_protocol_test::testy::TestyCommand;
use agent_client_protocol_tokio::AcpAgent;
use expect_test::expect;
use futures::StreamExt;
use futures::channel::mpsc;
use std::collections::HashMap;
use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Normalize events for stable snapshot testing.
///
/// - Strips timestamps (set to 0.0)
/// - Replaces UUIDs with sequential IDs (id:0, id:1, etc.)
/// - Replaces session IDs with "session:0", etc.
struct EventNormalizer {
    id_map: HashMap<String, String>,
    next_id: usize,
    session_map: HashMap<String, String>,
    next_session: usize,
}

impl EventNormalizer {
    fn new() -> Self {
        Self {
            id_map: HashMap::new(),
            next_id: 0,
            session_map: HashMap::new(),
            next_session: 0,
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

    /// Recursively normalize session IDs in JSON values.
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

#[tokio::test]
async fn test_trace_snapshot() -> Result<(), agent_client_protocol_core::Error> {
    // Create channel for collecting trace events
    let (tx, rx) = mpsc::unbounded();

    // Create the component chain: arrow_proxy -> eliza
    // Uses pre-built binaries to avoid cargo run races during `cargo test --all`
    let arrow_proxy_agent =
        AcpAgent::from_args([arrow_proxy_example().to_string_lossy().to_string()])?;
    let eliza_agent = testy();

    // Create duplex streams for editor <-> conductor communication
    let (editor_write, conductor_read) = duplex(8192);
    let (conductor_write, editor_read) = duplex(8192);

    // Spawn the conductor with tracing to the channel
    let conductor_handle = tokio::spawn(async move {
        ConductorImpl::new_agent(
            "conductor".to_string(),
            ProxiesAndAgent::new(eliza_agent).proxy(arrow_proxy_agent),
            Default::default(),
        )
        .trace_to(tx)
        .run(agent_client_protocol_core::ByteStreams::new(
            conductor_write.compat_write(),
            conductor_read.compat(),
        ))
        .await
    });

    // Run a simple prompt through the conductor
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        agent_client_protocol_yopo::prompt(
            agent_client_protocol_core::ByteStreams::new(
                editor_write.compat_write(),
                editor_read.compat(),
            ),
            TestyCommand::Greet.to_prompt(),
        )
        .await
    })
    .await
    .expect("Test timed out")?;

    // Abort the conductor to close the trace channel
    conductor_handle.abort();

    // Collect and normalize events
    let mut normalizer = EventNormalizer::new();
    let events = normalizer.normalize_events(rx.collect().await);

    // Snapshot the trace events
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
                        "cwd": String("."),
                        "mcpServers": Array [],
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
                    id: String("id:2"),
                    method: "session/prompt",
                    session: None,
                    params: Object {
                        "prompt": Array [
                            Object {
                                "text": String("{\"command\":\"greet\"}"),
                                "type": String("text"),
                            },
                        ],
                        "sessionId": String("session:0"),
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
                                "text": String("Hello, world!"),
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
                    id: String("id:2"),
                    is_error: false,
                    payload: Object {
                        "stopReason": String("end_turn"),
                    },
                },
            ),
        ]
    "#]]
    .assert_debug_eq(&events);

    println!("Response: {result}");

    Ok(())
}
