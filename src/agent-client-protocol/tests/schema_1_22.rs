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
        Agent, Channel, Client, DynConnectTo, Error, NullRun, mcp_admission,
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
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use tokio::sync::{Notify, oneshot};

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
        fn admit(&self, request: McpAdmission) -> BoxFuture<'static, Result<(), McpRejection>> {
            Box::pin(async move {
                match request.session_id() {
                    Some(session_id) if session_id.0.as_ref() == "child" => Ok(()),
                    _ => Err(McpRejection::session_rejected(
                        "not a session of this service",
                    )),
                }
            })
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

    /// The check of a server that needs the session of every request. It
    /// needs no `await`.
    async fn require_session(request: McpAdmission) -> Result<(), McpRejection> {
        request.require_session_id()?;
        Ok(())
    }

    /// The state of a check that waits, for a request without a session,
    /// until the test releases it, and then rejects the request.
    #[derive(Clone, Default)]
    struct Gate {
        /// Notified when a check starts to wait.
        waiting: Arc<Notify>,
        /// Notified by the test to let the waiting check decide.
        release: Arc<Notify>,
        /// Whether the future of a check that waited is gone.
        dropped: Arc<AtomicBool>,
    }

    impl Gate {
        async fn check(&self, request: McpAdmission) -> Result<(), McpRejection> {
            struct Dropped<'a>(&'a AtomicBool);
            impl Drop for Dropped<'_> {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }

            if request.session_id().is_some() {
                return Ok(());
            }
            let _dropped = Dropped(&self.dropped);
            self.waiting.notify_one();
            self.release.notified().await;
            Err(McpRejection::session_required())
        }
    }

    /// A server that counts its runs in `runs` and asks `gate` first. The
    /// check owns its copy of the gate and borrows it for each request.
    fn gated_server(runs: &Arc<AtomicUsize>, gate: &Gate) -> McpServer<Agent, NullRun> {
        let gate = gate.clone();
        counting_server(runs).with_admission(
            async move |request| gate.check(request).await,
            mcp_admission!(),
        )
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
        /// The ID of the outer `mcp/message` request.
        id: &'static str,
        /// The logical `requestId`.
        request_id: &'static str,
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
                request_id: "call",
                method,
                params: inner(json!({})),
                attribution,
            }
        }
    }

    /// The Agent side of a connection, once its session is set up.
    struct AgentSide<'a> {
        peer: &'a mut Channel,
        /// The server ID the Client declared in the session setup request.
        server_id: Value,
    }

    impl<'a> AgentSide<'a> {
        /// Answers the session setup request.
        async fn set_up(peer: &'a mut Channel) -> Self {
            let setup = receive(peer).await;
            let server_id = setup["params"]["mcpServers"][0]["serverId"].clone();
            send(
                peer,
                json!({"jsonrpc": "2.0", "id": setup["id"], "result": {"sessionId": "parent"}}),
            );
            Self { peer, server_id }
        }

        /// Sends `call` as one `mcp/message` request.
        fn send(&self, call: &Call) {
            let mut envelope = json!({
                "serverId": self.server_id,
                "requestId": call.request_id,
                "method": call.method,
                "params": call.params,
            });
            envelope
                .as_object_mut()
                .unwrap()
                .extend(call.attribution.as_object().unwrap().clone());
            send(
                self.peer,
                json!({"jsonrpc": "2.0", "id": call.id, "method": "mcp/message", "params": envelope}),
            );
        }

        /// Cancels the request with the outer ID `id`.
        fn cancel(&self, id: &str) {
            send(
                self.peer,
                json!({"jsonrpc": "2.0", "method": "$/cancel_request", "params": {"requestId": id}}),
            );
        }

        /// The next response, which has to answer the request `id`.
        async fn response(&mut self, id: &str) -> Value {
            let response = receive(self.peer).await;
            assert_eq!(response["id"], id, "{response}");
            response
        }

        /// Sends `call` and returns its response.
        async fn call(&mut self, call: &Call) -> Value {
            self.send(call);
            self.response(call.id).await
        }
    }

    async fn v1_session<T>(
        server: McpServer<Agent, NullRun>,
        script: impl AsyncFnOnce(&mut AgentSide<'_>) -> T,
    ) -> T {
        tokio::time::timeout(TIMEOUT, async {
            let (transport, mut peer) = Channel::duplex();
            let (stop_tx, stop_rx) = oneshot::channel();
            let client = tokio::spawn(Client.v1().connect_with(transport, async move |cx| {
                // Install the active session route before the first MCP request,
                // and keep its update stream open but unread.
                let _setup = cx
                    .build_session_cwd()?
                    .with_mcp_server(server)?
                    .block_task()
                    .start_session()
                    .await?;
                stop_rx.await.unwrap();
                Ok(())
            }));
            let output = script(&mut AgentSide::set_up(&mut peer).await).await;
            stop_tx.send(()).unwrap();
            client.await.unwrap().unwrap();
            output
        })
        .await
        .expect("the v1 MCP exchange timed out")
    }

    #[cfg(feature = "unstable_protocol_v2")]
    async fn v2_session<T>(
        server: McpServer<Agent, NullRun>,
        script: impl AsyncFnOnce(&mut AgentSide<'_>) -> T,
    ) -> T {
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
            let output = script(&mut AgentSide::set_up(&mut peer).await).await;
            stop_tx.send(()).unwrap();
            client.await.unwrap().unwrap();
            output
        })
        .await
        .expect("the v2 MCP exchange timed out")
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

    /// Attaches `server` to a session of `version` and runs `script` as the
    /// Agent.
    async fn session<T>(
        version: Version,
        server: McpServer<Agent, NullRun>,
        script: impl AsyncFnOnce(&mut AgentSide<'_>) -> T,
    ) -> T {
        match version {
            Version::V1 => v1_session(server, script).await,
            #[cfg(feature = "unstable_protocol_v2")]
            Version::V2 => v2_session(server, script).await,
        }
    }

    /// Attaches `server` to a session of `version`, sends `calls` in order,
    /// and returns the declared server ID and the responses.
    async fn run(
        version: Version,
        server: McpServer<Agent, NullRun>,
        calls: &[Call],
    ) -> (Value, Vec<Value>) {
        session(version, server, async |agent| {
            let mut responses = Vec::new();
            for call in calls {
                responses.push(agent.call(call).await);
            }
            (agent.server_id.clone(), responses)
        })
        .await
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
                json!({"sessionId": "parent"}),
                json!({"sessionId": "parent", "toolCallId": null}),
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

    /// No provider claims this server ID, so the session handler sees the
    /// request regardless of the order in which the dynamic handlers run.
    #[tokio::test]
    async fn active_session_does_not_claim_requests_for_an_unknown_mcp_server() {
        v1_session(server(), async |agent| {
            agent.server_id = json!("unregistered");
            let call = Call::new("tools/list", json!({"sessionId": "parent"}));
            let response = agent.call(&call).await;
            assert_eq!(response["error"]["code"], -33001, "{response}");
        })
        .await;
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
            let server = counting_server(&runs).with_admission(require_session, mcp_admission!());
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
            let server = counting_server(&runs).with_admission(require_session, mcp_admission!());
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
            let server = server().with_admission(
                {
                    let seen = seen.clone();
                    async move |request| {
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
                            return Err(McpRejection::session_rejected(
                                "the session is not active",
                            ));
                        }
                        Ok(())
                    }
                },
                mcp_admission!(),
            );
            let write = |request_id, attribution| Call {
                request_id,
                params: inner(json!({"name": "write"})),
                ..Call::new("tools/call", attribution)
            };
            let calls = [
                write("unattributed", json!({})),
                // Even the active session's ID is attribution, not a route
                // to its update stream. The provider gets to reject it.
                write(
                    "refused",
                    json!({"sessionId": "parent", "toolCallId": "call_1"}),
                ),
                write("accepted", json!({"sessionId": "child"})),
                // The rule is for one tool, so other requests need no session.
                Call {
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
                    "sessionId": "parent",
                    "toolCallId": "call_1",
                }),
                "{version:?}"
            );
            // Only `tools/call` has a tool name.
            assert_eq!(seen[4]["method"], "tools/list");
            assert_eq!(seen[4]["toolName"], Value::Null);
        }
    }

    /// A check can await before it decides, for example to read session
    /// state that lives behind an async lock. It owns the state it captured
    /// and borrows it for each request.
    #[tokio::test]
    async fn admission_check_can_await_before_it_decides() {
        for &version in VERSIONS {
            let runs = Arc::new(AtomicUsize::new(0));
            let active_sessions = tokio::sync::Mutex::new(vec!["child".to_owned()]);
            let server = counting_server(&runs).with_admission(
                async move |request| {
                    let session_id = request.require_session_id()?;
                    tokio::task::yield_now().await;
                    let active = active_sessions.lock().await;
                    if active.iter().any(|id| id == session_id.0.as_ref()) {
                        Ok(())
                    } else {
                        Err(McpRejection::session_rejected("the session is not active"))
                    }
                },
                mcp_admission!(),
            );
            let calls = [
                Call::new("tools/call", json!({})),
                Call::new("tools/call", json!({"sessionId": "other"})),
                Call::new("tools/call", json!({"sessionId": "child"})),
            ];
            let (_, responses) = run(version, server, &calls).await;
            assert_rejected(&responses[0], SESSION_REQUIRED);
            assert_rejected(
                &responses[1],
                "sessionId rejected: the session is not active",
            );
            assert_eq!(
                responses[2]["result"]["result"]["sessionId"], "child",
                "{version:?}"
            );
            assert_eq!(runs.load(Ordering::SeqCst), 1, "{version:?}");
        }
    }

    /// Each `with_admission` call adds a check. They run in the order they
    /// were added. The first rejection answers the request, and the checks
    /// after it do not run.
    #[tokio::test]
    async fn admission_checks_run_in_the_order_they_were_added() {
        for &version in VERSIONS {
            let runs = Arc::new(AtomicUsize::new(0));
            let second_asked = Arc::new(AtomicUsize::new(0));
            let server = counting_server(&runs)
                .with_admission(require_session, mcp_admission!())
                .with_admission(
                    {
                        let second_asked = second_asked.clone();
                        async move |_request| {
                            second_asked.fetch_add(1, Ordering::SeqCst);
                            Err(McpRejection::session_rejected("second check"))
                        }
                    },
                    mcp_admission!(),
                );
            let calls = [
                Call::new("tools/call", json!({})),
                Call::new("tools/call", json!({"sessionId": "child"})),
            ];
            let (_, responses) = run(version, server, &calls).await;
            assert_rejected(&responses[0], SESSION_REQUIRED);
            assert_rejected(&responses[1], "sessionId rejected: second check");
            assert_eq!(second_asked.load(Ordering::SeqCst), 1, "{version:?}");
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
                .with_admission(require_session, mcp_admission!());
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
            let server = McpServer::new(connector, NullRun)
                .with_admission(require_session, mcp_admission!());
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
            let server = server().with_admission(
                {
                    let asked = asked.clone();
                    async move |_request| {
                        asked.fetch_add(1, Ordering::SeqCst);
                        Err(McpRejection::session_required())
                    }
                },
                mcp_admission!(),
            );
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

    /// A request holds its `requestId` while its check is pending, so a
    /// second request with that ID is a duplicate. A rejection frees the ID.
    #[tokio::test]
    async fn pending_admission_check_holds_the_request_id_until_it_decides() {
        for &version in VERSIONS {
            let runs = Arc::new(AtomicUsize::new(0));
            let gate = Gate::default();
            let server = gated_server(&runs, &gate);
            session(version, server, async |agent| {
                let pending = Call {
                    id: "pending",
                    ..Call::new("tools/call", json!({}))
                };
                agent.send(&pending);
                gate.waiting.notified().await;

                let duplicate = Call {
                    id: "duplicate",
                    ..Call::new("tools/call", json!({"sessionId": "child"}))
                };
                let response = agent.call(&duplicate).await;
                assert_rejected(&response, "duplicate active MCP requestId");

                gate.release.notify_one();
                assert_rejected(&agent.response("pending").await, SESSION_REQUIRED);
                assert_eq!(runs.load(Ordering::SeqCst), 0, "{version:?}");

                let response = agent.call(&duplicate).await;
                assert_eq!(
                    response["result"]["result"]["sessionId"], "child",
                    "{version:?}"
                );
            })
            .await;
            assert_eq!(runs.load(Ordering::SeqCst), 1, "{version:?}");
        }
    }

    /// Cancelling a request whose check is pending ends it with Request
    /// cancelled. The check's future is dropped, nothing runs, and the
    /// `requestId` is free again.
    #[tokio::test]
    async fn cancelling_a_request_ends_its_pending_admission_check() {
        for &version in VERSIONS {
            let runs = Arc::new(AtomicUsize::new(0));
            let gate = Gate::default();
            let server = gated_server(&runs, &gate);
            session(version, server, async |agent| {
                agent.send(&Call::new("tools/call", json!({})));
                gate.waiting.notified().await;
                assert!(!gate.dropped.load(Ordering::SeqCst), "{version:?}");

                agent.cancel("call");
                let response = agent.response("call").await;
                assert_eq!(
                    response.get("error"),
                    Some(&json!({"code": -32800, "message": "Request cancelled"})),
                    "{version:?}"
                );
                assert!(gate.dropped.load(Ordering::SeqCst), "{version:?}");
                assert_eq!(runs.load(Ordering::SeqCst), 0, "{version:?}");

                let call = Call::new("tools/call", json!({"sessionId": "child"}));
                let response = agent.call(&call).await;
                assert_eq!(
                    response["result"]["result"]["sessionId"], "child",
                    "{version:?}"
                );
            })
            .await;
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
            let server = server().with_admission(require_session, mcp_admission!());
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

    /// An application tests its own check with a request it builds itself.
    #[test]
    fn admission_check_can_be_tested_without_a_connection() {
        let request = McpAdmission::new("server-1", "request-1", "tools/call");
        assert_eq!(request.server_id().0.as_ref(), "server-1");
        assert_eq!(request.request_id().0.as_ref(), "request-1");
        assert_eq!(request.method(), "tools/call");
        assert_eq!(request.params(), None);
        assert_eq!(request.tool_name(), None);
        assert_eq!(request.session_id(), None);
        assert_eq!(request.tool_call_id(), None);
        assert_eq!(
            futures::executor::block_on(require_session(request.clone())),
            Err(McpRejection::session_required())
        );

        let params = inner(json!({"name": "write"}));
        let request = request
            .with_params(params.as_object().unwrap().clone())
            .with_session_id("sess_1")
            .with_tool_call_id("call_1");
        assert_eq!(request.params(), params.as_object());
        assert_eq!(request.tool_name(), Some("write"));
        assert_eq!(request.session_id().map(|id| id.0.as_ref()), Some("sess_1"));
        assert_eq!(
            request.tool_call_id().map(|id| id.0.as_ref()),
            Some("call_1")
        );
        assert_eq!(
            futures::executor::block_on(require_session(request)),
            Ok(())
        );
    }
}
