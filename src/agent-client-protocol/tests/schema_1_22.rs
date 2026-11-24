//! Checks the changes from schema 1.22. Two malformed v1 payloads get Invalid
//! params (`-32602`) instead of being read as something else, the session and
//! tool call of an `mcp/message` request reach the MCP service, and a server's
//! admission check can reject a request for its session before it runs.

use agent_client_protocol::{
    Agent, Channel, Client, Responder, TransportFrame, V1,
    v1::{ConnectionTo, schema as v1},
};
use futures::StreamExt as _;
use serde_json::{Value, json};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

fn send(peer: &Channel, message: Value) {
    peer.tx
        .unbounded_send(TransportFrame::Single(
            serde_json::from_value(message).unwrap(),
        ))
        .unwrap();
}

async fn receive(peer: &mut Channel) -> Value {
    let frame = tokio::time::timeout(TIMEOUT, peer.rx.next())
        .await
        .expect("timed out waiting for a message");
    let Some(TransportFrame::Single(message)) = frame else {
        panic!("expected a single frame")
    };
    serde_json::to_value(message).unwrap()
}

/// Sends one request to a v1 Agent that accepts every `session/new` and
/// `session/set_config_option` request it can parse, and returns the response.
async fn v1_agent_response(method: &str, params: Value) -> Value {
    let (transport, mut peer) = Channel::duplex();
    let agent = Agent
        .v1()
        .on_receive_request(
            async |_request: v1::NewSessionRequest,
                   responder: Responder<v1::NewSessionResponse, V1>,
                   _connection: ConnectionTo<Client>| {
                responder.respond(v1::NewSessionResponse::new("accepted"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_request: v1::SetSessionConfigOptionRequest,
                   responder: Responder<v1::SetSessionConfigOptionResponse, V1>,
                   _connection: ConnectionTo<Client>| {
                responder.respond(v1::SetSessionConfigOptionResponse::new(Vec::new()))
            },
            agent_client_protocol::on_receive_request!(),
        );
    let agent = tokio::spawn(agent.connect_to(transport));
    send(
        &peer,
        json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}),
    );
    let response = receive(&mut peer).await;
    drop(peer);
    agent.await.unwrap().unwrap();
    response
}

/// A server with a known remote `type` that fails its own schema is rejected.
/// It is not launched as a stdio server from its `command`.
#[tokio::test]
async fn v1_malformed_remote_mcp_server_gets_invalid_params() {
    let stdio = json!({"name": "local", "command": "/usr/bin/server", "args": [], "env": []});
    let mut typed_stdio = stdio.clone();
    typed_stdio["type"] = json!("stdio");
    for accepted in [stdio.clone(), typed_stdio] {
        let response = v1_agent_response(
            "session/new",
            json!({"cwd": "/work", "mcpServers": [accepted]}),
        )
        .await;
        assert_eq!(response["result"]["sessionId"], "accepted", "{response}");
    }

    for remote in ["http", "sse"] {
        let mut malformed = stdio.clone();
        malformed["type"] = json!(remote);
        malformed["url"] = json!(42);
        malformed["headers"] = json!([]);
        let response = v1_agent_response(
            "session/new",
            json!({"cwd": "/work", "mcpServers": [malformed]}),
        )
        .await;
        assert_eq!(response["error"]["code"], -32602, "{remote}: {response}");
    }
}

/// A `boolean` config value with a string payload is rejected. It is not read
/// as the ID of a select value.
#[tokio::test]
async fn v1_boolean_config_value_with_a_string_gets_invalid_params() {
    let set = |extra: Value| {
        let mut params = json!({"sessionId": "s", "configId": "fast"});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        v1_agent_response("session/set_config_option", params)
    };

    for accepted in [
        json!({"value": "on"}),
        json!({"type": "boolean", "value": true}),
    ] {
        let response = set(accepted.clone()).await;
        assert_eq!(
            response["result"]["configOptions"],
            json!([]),
            "{accepted}: {response}"
        );
    }

    let response = set(json!({"type": "boolean", "value": "false"})).await;
    assert_eq!(response["error"]["code"], -32602, "{response}");
}

#[cfg(feature = "unstable_mcp_over_acp")]
mod mcp_attribution {
    use super::{TIMEOUT, receive, send};
    use agent_client_protocol::{
        Agent, Channel, Client, DynConnectTo, Error, NullRun,
        mcp_server::{
            McpAdmission, McpConnectionTo, McpOutcome, McpRejection, McpRequest, McpRequestContext,
            McpServer, McpServerConnect, McpService,
        },
        role,
    };
    #[cfg(feature = "unstable_protocol_v2")]
    use agent_client_protocol::{schema::ProtocolVersion, v2::schema as v2};
    use futures::future::BoxFuture;
    use serde_json::{Value, json};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::oneshot;

    /// Counts the requests it runs, and answers `tools/call` with the session
    /// and tool call the request was sent for.
    #[derive(Default)]
    struct Service {
        runs: Arc<AtomicUsize>,
    }

    impl McpService<Agent> for Service {
        fn execute(
            &self,
            request: McpRequest,
            context: McpRequestContext<Agent>,
        ) -> BoxFuture<'static, Result<McpOutcome, Error>> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            // A `tool_fn` closure receives `context.connection()`, so check
            // that it carries the same values.
            let connection = context.connection();
            assert_eq!(context.session_id(), connection.session_id());
            assert_eq!(context.tool_call_id(), connection.tool_call_id());
            let result = match request.method.as_str() {
                "tools/call" => Ok(json!({
                    "sessionId": context.session_id(),
                    "toolCallId": context.tool_call_id(),
                })),
                "tools/list" => Ok(json!({"tools": []})),
                "server/discover" => Ok(json!({"supportedVersions": ["2026-07-28"]})),
                "test/fail" => Err(Error::internal_error().data("database unavailable")),
                "test/invalid_params" => Err(Error::invalid_params().data("from the service")),
                method => panic!("unexpected test method {method}"),
            };
            Box::pin(async move { result.map(McpOutcome::Result) })
        }
    }

    /// A service that states its own admission rule.
    struct ChildSessionsOnly(Service);

    impl McpService<Agent> for ChildSessionsOnly {
        fn admit(&self, request: &McpAdmission<'_>) -> Result<(), McpRejection> {
            match request.session_id() {
                Some(session_id) if session_id.0.as_ref() == "child" => Ok(()),
                _ => Err(McpRejection::session_rejected(
                    "not a session of this service",
                )),
            }
        }

        fn execute(
            &self,
            request: McpRequest,
            context: McpRequestContext<Agent>,
        ) -> BoxFuture<'static, Result<McpOutcome, Error>> {
            self.0.execute(request, context)
        }
    }

    /// A connector-based server that counts the backends it is asked for.
    struct CountingConnector {
        connects: Arc<AtomicUsize>,
    }

    impl McpServerConnect<Agent> for CountingConnector {
        fn name(&self) -> String {
            "connector".into()
        }

        fn connect(&self, _cx: McpConnectionTo<Agent>) -> DynConnectTo<role::mcp::Client> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            panic!("a rejected request must not reach the connector")
        }
    }

    fn server() -> McpServer<Agent, NullRun> {
        McpServer::new_service("attribution", Service::default(), NullRun)
    }

    /// A server that counts its runs in `runs`.
    fn counting_server(runs: &Arc<AtomicUsize>) -> McpServer<Agent, NullRun> {
        McpServer::new_service("attribution", Service { runs: runs.clone() }, NullRun)
    }

    /// The check of a server that needs the session of every request.
    fn require_session(request: &McpAdmission<'_>) -> Result<(), McpRejection> {
        request.require_session_id()?;
        Ok(())
    }

    /// The inner params of a request: `extra` and the `_meta` every modern
    /// MCP request carries.
    fn inner(mut extra: Value) -> Value {
        extra["_meta"] = json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {}
        });
        extra
    }

    /// One `mcp/message` request for the Agent side of a test to send.
    #[derive(Clone)]
    struct Call {
        /// The logical `requestId`, also used as the outer request ID.
        id: &'static str,
        /// The inner MCP method.
        method: &'static str,
        /// The inner MCP params.
        params: Value,
        /// The `sessionId` and `toolCallId` to merge into the envelope.
        attribution: Value,
    }

    impl Call {
        fn new(method: &'static str, attribution: Value) -> Self {
            Self {
                id: "call",
                method,
                params: inner(json!({})),
                attribution,
            }
        }
    }

    /// Plays the Agent on `peer`: answers the session setup request, sends
    /// `calls` in order as `mcp/message` requests, and returns the declared
    /// server ID and the responses.
    async fn exchange(peer: &mut Channel, calls: &[Call]) -> (Value, Vec<Value>) {
        let setup = receive(peer).await;
        let server_id = setup["params"]["mcpServers"][0]["serverId"].clone();
        send(
            peer,
            json!({"jsonrpc": "2.0", "id": setup["id"], "result": {"sessionId": "parent"}}),
        );
        let mut responses = Vec::new();
        for call in calls {
            let mut envelope = json!({
                "serverId": server_id,
                "requestId": call.id,
                "method": call.method,
                "params": call.params,
            });
            envelope
                .as_object_mut()
                .unwrap()
                .extend(call.attribution.as_object().unwrap().clone());
            send(
                peer,
                json!({"jsonrpc": "2.0", "id": call.id, "method": "mcp/message", "params": envelope}),
            );
            let response = receive(peer).await;
            assert_eq!(response["id"], call.id, "{response}");
            responses.push(response);
        }
        (server_id, responses)
    }

    async fn v1_calls(server: McpServer<Agent, NullRun>, calls: &[Call]) -> (Value, Vec<Value>) {
        tokio::time::timeout(TIMEOUT, async {
            let (transport, mut peer) = Channel::duplex();
            let (stop_tx, stop_rx) = oneshot::channel();
            let client = tokio::spawn(Client.v1().connect_with(transport, async move |cx| {
                cx.build_session_cwd()?
                    .with_mcp_server(server)?
                    .block_task()
                    .run_until(async |_session| {
                        stop_rx.await.unwrap();
                        Ok(())
                    })
                    .await
            }));
            let exchanged = exchange(&mut peer, calls).await;
            stop_tx.send(()).unwrap();
            client.await.unwrap().unwrap();
            exchanged
        })
        .await
        .expect("the v1 MCP calls timed out")
    }

    #[cfg(feature = "unstable_protocol_v2")]
    async fn v2_calls(server: McpServer<Agent, NullRun>, calls: &[Call]) -> (Value, Vec<Value>) {
        tokio::time::timeout(TIMEOUT, async {
            let (transport, mut peer) = Channel::duplex();
            let (stop_tx, stop_rx) = oneshot::channel();
            let implementation = || v2::Implementation::new("schema-1-22-test", "1");
            let client = tokio::spawn(Client.v2().connect_with(transport, async move |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;
                // The attachment lives as long as the connection.
                let _setup = cx
                    .build_session_cwd()?
                    .with_mcp_server(server)?
                    .start_session()
                    .block_task()
                    .await?;
                stop_rx.await.unwrap();
                Ok(())
            }));
            let initialize = receive(&mut peer).await;
            assert_eq!(initialize["method"], "initialize", "{initialize}");
            let capabilities = v2::AgentCapabilities::new().session(
                v2::SessionCapabilities::new()
                    .mcp(v2::McpCapabilities::new().acp(v2::McpAcpCapabilities::new())),
            );
            send(
                &peer,
                json!({
                    "jsonrpc": "2.0",
                    "id": initialize["id"],
                    "result": v2::InitializeResponse::new(ProtocolVersion::V2, implementation())
                        .capabilities(capabilities),
                }),
            );
            let exchanged = exchange(&mut peer, calls).await;
            stop_tx.send(()).unwrap();
            client.await.unwrap().unwrap();
            exchanged
        })
        .await
        .expect("the v2 MCP calls timed out")
    }

    /// The protocol versions this build can attach a server to.
    #[derive(Clone, Copy, Debug)]
    enum Version {
        V1,
        #[cfg(feature = "unstable_protocol_v2")]
        V2,
    }

    const VERSIONS: &[Version] = &[
        Version::V1,
        #[cfg(feature = "unstable_protocol_v2")]
        Version::V2,
    ];

    /// Attaches `server` to a session of `version`, sends `calls` in order,
    /// and returns the declared server ID and the responses.
    async fn run(
        version: Version,
        server: McpServer<Agent, NullRun>,
        calls: &[Call],
    ) -> (Value, Vec<Value>) {
        match version {
            Version::V1 => v1_calls(server, calls).await,
            #[cfg(feature = "unstable_protocol_v2")]
            Version::V2 => v2_calls(server, calls).await,
        }
    }

    /// What the service sees for each form of attribution the Agent can send.
    fn cases() -> [(Value, Value); 4] {
        [
            (json!({}), json!({"sessionId": null, "toolCallId": null})),
            (
                json!({"sessionId": null, "toolCallId": null}),
                json!({"sessionId": null, "toolCallId": null}),
            ),
            (
                json!({"sessionId": "child"}),
                json!({"sessionId": "child", "toolCallId": null}),
            ),
            (
                json!({"sessionId": "child", "toolCallId": "call_1"}),
                json!({"sessionId": "child", "toolCallId": "call_1"}),
            ),
        ]
    }

    /// A server without an admission check runs every request, with or
    /// without a session.
    #[tokio::test]
    async fn service_sees_the_session_and_tool_call_of_a_request() {
        for &version in VERSIONS {
            for (attribution, seen) in cases() {
                let call = Call::new("tools/call", attribution.clone());
                let (_, responses) = run(version, server(), &[call]).await;
                assert_eq!(
                    responses[0]["result"]["result"], seen,
                    "{version:?} {attribution}"
                );
            }
            // A `sessionId` that is not a string is a malformed envelope.
            let call = Call::new("tools/call", json!({"sessionId": 42}));
            let (_, responses) = run(version, server(), &[call]).await;
            assert_eq!(responses[0]["error"]["code"], -32602, "{version:?}");
        }
    }

    /// Whether `response` is the outer error Invalid params with `reason`.
    #[track_caller]
    fn assert_rejected(response: &Value, reason: &str) {
        assert_eq!(
            response.get("error"),
            Some(&json!({"code": -32602, "message": "Invalid params", "data": reason})),
            "{response}"
        );
        assert_eq!(response.get("result"), None, "{response}");
    }

    const SESSION_REQUIRED: &str = "this MCP server requires sessionId on mcp/message";

    /// The check runs for every method. A rejected request does not reach
    /// the service, and its `requestId` is free for the next request.
    #[tokio::test]
    async fn admission_check_rejects_a_request_before_it_runs() {
        let child = json!({"sessionId": "child"});
        for &version in VERSIONS {
            let runs = Arc::new(AtomicUsize::new(0));
            let rejected = ["tools/call", "tools/list", "server/discover"]
                .map(|method| Call::new(method, json!({})));
            let server = counting_server(&runs).with_admission(require_session);
            let (_, responses) = run(version, server, &rejected).await;
            for response in &responses {
                assert_rejected(response, SESSION_REQUIRED);
            }
            assert_eq!(runs.load(Ordering::SeqCst), 0, "{version:?}");

            // The same `requestId` as the rejected request before it.
            let calls = [
                Call::new("tools/call", json!({})),
                Call::new("tools/call", child.clone()),
                Call::new("tools/list", child.clone()),
                Call::new("server/discover", child.clone()),
            ];
            let server = counting_server(&runs).with_admission(require_session);
            let (_, responses) = run(version, server, &calls).await;
            assert_rejected(&responses[0], SESSION_REQUIRED);
            assert_eq!(
                responses[1]["result"]["result"],
                json!({"sessionId": "child", "toolCallId": null}),
                "{version:?}"
            );
            assert_eq!(
                responses[2]["result"]["result"],
                json!({"tools": []}),
                "{version:?}"
            );
            assert_eq!(
                responses[3]["result"]["result"],
                json!({"supportedVersions": ["2026-07-28"]}),
                "{version:?}"
            );
            assert_eq!(runs.load(Ordering::SeqCst), 3, "{version:?}");
        }
    }

    /// The check reads the envelope and the inner request, so it can apply a
    /// rule to one tool and refuse a session it does not accept.
    #[tokio::test]
    async fn admission_check_reads_the_request_it_decides_on() {
        for &version in VERSIONS {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let server = server().with_admission({
                let seen = seen.clone();
                move |request| {
                    seen.lock().unwrap().push(json!({
                        "serverId": request.server_id(),
                        "requestId": request.request_id(),
                        "method": request.method(),
                        "toolName": request.tool_name(),
                        "params": request.params(),
                        "sessionId": request.session_id(),
                        "toolCallId": request.tool_call_id(),
                    }));
                    if request.tool_name() == Some("write")
                        && request.require_session_id()?.0.as_ref() != "child"
                    {
                        return Err(McpRejection::session_rejected("the session is not active"));
                    }
                    Ok(())
                }
            });
            let write = |id, attribution| Call {
                id,
                params: inner(json!({"name": "write"})),
                ..Call::new("tools/call", attribution)
            };
            let calls = [
                write("unattributed", json!({})),
                write(
                    "refused",
                    json!({"sessionId": "other", "toolCallId": "call_1"}),
                ),
                write("accepted", json!({"sessionId": "child"})),
                // The rule is for one tool, so other requests need no session.
                Call {
                    id: "read",
                    params: inner(json!({"name": "read"})),
                    ..Call::new("tools/call", json!({}))
                },
                Call::new("tools/list", json!({})),
            ];
            let (server_id, responses) = run(version, server, &calls).await;
            assert_rejected(&responses[0], SESSION_REQUIRED);
            assert_rejected(
                &responses[1],
                "sessionId rejected: the session is not active",
            );
            for admitted in &responses[2..] {
                assert!(admitted["result"]["result"].is_object(), "{admitted}");
            }

            let seen = seen.lock().unwrap();
            assert_eq!(
                seen[1],
                json!({
                    "serverId": server_id,
                    "requestId": "refused",
                    "method": "tools/call",
                    "toolName": "write",
                    "params": inner(json!({"name": "write"})),
                    "sessionId": "other",
                    "toolCallId": "call_1",
                }),
                "{version:?}"
            );
            // Only `tools/call` has a tool name.
            assert_eq!(seen[4]["method"], "tools/list");
            assert_eq!(seen[4]["toolName"], Value::Null);
        }
    }

    /// Each `with_admission` call adds a check. They run in the order they
    /// were added, and the first rejection answers the request.
    #[tokio::test]
    async fn admission_checks_run_in_the_order_they_were_added() {
        for &version in VERSIONS {
            let runs = Arc::new(AtomicUsize::new(0));
            let server = counting_server(&runs)
                .with_admission(require_session)
                .with_admission(|_request| Err(McpRejection::session_rejected("second check")));
            let calls = [
                Call::new("tools/call", json!({})),
                Call::new("tools/call", json!({"sessionId": "child"})),
            ];
            let (_, responses) = run(version, server, &calls).await;
            assert_rejected(&responses[0], SESSION_REQUIRED);
            assert_rejected(&responses[1], "sessionId rejected: second check");
            assert_eq!(runs.load(Ordering::SeqCst), 0, "{version:?}");
        }
    }

    /// A service's own `admit` runs after the server's checks, and rejects
    /// the same way.
    #[tokio::test]
    async fn service_admits_after_the_checks_of_its_server() {
        for &version in VERSIONS {
            let runs = Arc::new(AtomicUsize::new(0));
            let service = ChildSessionsOnly(Service { runs: runs.clone() });
            let server = McpServer::new_service("own-rule", service, NullRun)
                .with_admission(require_session);
            let calls = [
                Call::new("tools/call", json!({})),
                Call::new("tools/call", json!({"sessionId": "other"})),
                Call::new("tools/call", json!({"sessionId": "child"})),
            ];
            let (_, responses) = run(version, server, &calls).await;
            assert_rejected(&responses[0], SESSION_REQUIRED);
            assert_rejected(
                &responses[1],
                "sessionId rejected: not a session of this service",
            );
            assert_eq!(
                responses[2]["result"]["result"]["sessionId"], "child",
                "{version:?}"
            );
            assert_eq!(runs.load(Ordering::SeqCst), 1, "{version:?}");
        }
    }

    /// A rejected request does not reach a connector-based server either.
    #[tokio::test]
    async fn admission_check_rejects_before_a_connector_is_asked() {
        for &version in VERSIONS {
            let connects = Arc::new(AtomicUsize::new(0));
            let connector = CountingConnector {
                connects: connects.clone(),
            };
            let server = McpServer::new(connector, NullRun).with_admission(require_session);
            let calls = [Call::new("tools/call", json!({}))];
            let (_, responses) = run(version, server, &calls).await;
            assert_rejected(&responses[0], SESSION_REQUIRED);
            assert_eq!(connects.load(Ordering::SeqCst), 0, "{version:?}");
        }
    }

    /// A malformed inner request is an inner MCP error, as without a check.
    /// The check is not asked about it.
    #[tokio::test]
    async fn admission_check_is_not_asked_about_a_malformed_inner_request() {
        for &version in VERSIONS {
            let asked = Arc::new(AtomicUsize::new(0));
            let server = server().with_admission({
                let asked = asked.clone();
                move |_request| {
                    asked.fetch_add(1, Ordering::SeqCst);
                    Err(McpRejection::session_required())
                }
            });
            let without_meta = Call {
                params: json!({}),
                ..Call::new("tools/call", json!({}))
            };
            let (_, responses) = run(version, server, &[without_meta]).await;
            assert_eq!(responses[0].get("error"), None, "{version:?}");
            assert_eq!(
                responses[0]["result"]["error"],
                json!({
                    "code": -32602,
                    "message": "Invalid params",
                    "data": "inner params._meta must be an object",
                }),
                "{version:?}"
            );
            assert_eq!(asked.load(Ordering::SeqCst), 0, "{version:?}");
        }
    }

    /// An error a service returns is a backend failure, whatever its own
    /// code is. Only an admission check answers `-32602`.
    #[tokio::test]
    async fn service_errors_stay_backend_failures() {
        let child = json!({"sessionId": "child"});
        for &version in VERSIONS {
            let calls = [
                Call::new("test/fail", child.clone()),
                Call::new("test/invalid_params", child.clone()),
            ];
            let server = server().with_admission(require_session);
            let (_, responses) = run(version, server, &calls).await;
            for (response, data) in responses.iter().zip([
                "Internal error: \"database unavailable\"",
                "Invalid params: \"from the service\"",
            ]) {
                assert_eq!(
                    response.get("error"),
                    Some(&json!({
                        "code": -33002,
                        "message": "MCP backend failure",
                        "data": data,
                    })),
                    "{version:?}"
                );
            }
        }
    }
}
