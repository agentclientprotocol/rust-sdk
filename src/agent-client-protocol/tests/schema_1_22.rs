//! Checks the changes from schema 1.22. Two malformed v1 payloads get Invalid
//! params (`-32602`) instead of being read as something else, the session and
//! tool call of an `mcp/message` request reach the MCP service, and the
//! service can reject a request for its session with `-32602`.

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
    use super::{TIMEOUT, receive, send, v1};
    use agent_client_protocol::{
        Agent, Channel, Client, Error, NullRun,
        mcp_server::{McpOutcome, McpRequest, McpRequestContext, McpServer, McpService},
    };
    #[cfg(feature = "unstable_protocol_v2")]
    use agent_client_protocol::{schema::ProtocolVersion, v2::schema as v2};
    use futures::future::BoxFuture;
    use serde_json::{Value, json};
    use tokio::sync::oneshot;

    /// Answers `tools/call` with the session and tool call the request was
    /// sent for. Each `test/...` method ends the request in one other way.
    struct Service;

    impl McpService<Agent> for Service {
        fn execute(
            &self,
            request: McpRequest,
            context: McpRequestContext<Agent>,
        ) -> BoxFuture<'static, Result<McpOutcome, Error>> {
            Box::pin(async move {
                let session = |id: &v1::SessionId| McpOutcome::Result(json!({"sessionId": id}));
                match request.method.as_str() {
                    "tools/call" => {
                        // A `tool_fn` closure receives `context.connection()`,
                        // so check that it carries the same values.
                        let connection = context.connection();
                        assert_eq!(context.session_id(), connection.session_id());
                        assert_eq!(context.tool_call_id(), connection.tool_call_id());
                        Ok(McpOutcome::Result(json!({
                            "sessionId": context.session_id(),
                            "toolCallId": context.tool_call_id(),
                        })))
                    }
                    "test/require" => Ok(session(context.require_session_id()?)),
                    "test/require_on_connection" => {
                        Ok(session(context.connection().require_session_id()?))
                    }
                    "test/reject" => Err(context.reject_session("the session is not active")),
                    "test/reject_then_result" => {
                        let _rejection = context.reject_session("ignored by the service");
                        Ok(McpOutcome::Result(json!({"ran": true})))
                    }
                    "test/reject_then_mcp_error" => {
                        let _rejection = context.reject_session("ignored by the service");
                        Ok(McpOutcome::Error(v1::McpError::new(-32000, "tool failed")))
                    }
                    "test/reject_then_other_error" => {
                        let _rejection = context.reject_session("ignored by the service");
                        Err(Error::internal_error())
                    }
                    "test/reject_twice" => {
                        let _first = context.reject_session("first");
                        Err(context.reject_session("second"))
                    }
                    "test/reject_then_wait_for_cancellation" => {
                        let _rejection = context.reject_session("before the cancellation");
                        context.operation_cancellation().cancelled().await;
                        Ok(McpOutcome::Result(json!({"ran": true})))
                    }
                    "test/fail" => Err(Error::internal_error().data("database unavailable")),
                    "test/invalid_params_without_rejection" => {
                        Err(Error::invalid_params().data("not recorded as a rejection"))
                    }
                    method => panic!("unexpected test method {method}"),
                }
            })
        }
    }

    fn server() -> McpServer<Agent, NullRun> {
        McpServer::new_service("attribution", Service, NullRun)
    }

    /// One `mcp/message` request for the Agent side of a test to send.
    #[derive(Clone)]
    struct Call {
        /// The inner MCP method.
        method: &'static str,
        /// The `sessionId` and `toolCallId` to merge into the request's params.
        attribution: Value,
        /// Whether to cancel the request right after sending it.
        cancel: bool,
    }

    impl Call {
        fn new(method: &'static str, attribution: Value) -> Self {
            Self {
                method,
                attribution,
                cancel: false,
            }
        }
    }

    /// Plays the Agent on `peer`: answers the session setup request, sends
    /// `call` as one `mcp/message` request, and returns the response.
    async fn exchange(peer: &mut Channel, call: &Call) -> Value {
        let setup = receive(peer).await;
        let server_id = setup["params"]["mcpServers"][0]["serverId"].clone();
        send(
            peer,
            json!({"jsonrpc": "2.0", "id": setup["id"], "result": {"sessionId": "parent"}}),
        );
        let mut params = json!({
            "serverId": server_id,
            "requestId": "call",
            "method": call.method,
            "params": {"_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {}
            }}
        });
        params
            .as_object_mut()
            .unwrap()
            .extend(call.attribution.as_object().unwrap().clone());
        send(
            peer,
            json!({"jsonrpc": "2.0", "id": "call", "method": "mcp/message", "params": params}),
        );
        if call.cancel {
            send(
                peer,
                json!({"jsonrpc": "2.0", "method": "$/cancel_request", "params": {"requestId": "call"}}),
            );
        }
        let response = receive(peer).await;
        assert_eq!(response["id"], "call", "{response}");
        response
    }

    async fn v1_call(call: Call) -> Value {
        tokio::time::timeout(TIMEOUT, async {
            let (transport, mut peer) = Channel::duplex();
            let (stop_tx, stop_rx) = oneshot::channel();
            let client = tokio::spawn(Client.v1().connect_with(transport, async move |cx| {
                cx.build_session_cwd()?
                    .with_mcp_server(server())?
                    .block_task()
                    .run_until(async |_session| {
                        stop_rx.await.unwrap();
                        Ok(())
                    })
                    .await
            }));
            let response = exchange(&mut peer, &call).await;
            stop_tx.send(()).unwrap();
            client.await.unwrap().unwrap();
            response
        })
        .await
        .expect("the v1 MCP call timed out")
    }

    #[cfg(feature = "unstable_protocol_v2")]
    async fn v2_call(call: Call) -> Value {
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
                    .with_mcp_server(server())?
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
            let response = exchange(&mut peer, &call).await;
            stop_tx.send(()).unwrap();
            client.await.unwrap().unwrap();
            response
        })
        .await
        .expect("the v2 MCP call timed out")
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

    #[tokio::test]
    async fn v1_service_sees_the_session_and_tool_call_of_a_request() {
        for (attribution, seen) in cases() {
            let response = v1_call(Call::new("tools/call", attribution.clone())).await;
            assert_eq!(response["result"]["result"], seen, "{attribution}");
        }
        // A `sessionId` that is not a string is a malformed envelope.
        let response = v1_call(Call::new("tools/call", json!({"sessionId": 42}))).await;
        assert_eq!(response["error"]["code"], -32602, "{response}");
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[tokio::test]
    async fn v2_service_sees_the_session_and_tool_call_of_a_request() {
        for (attribution, seen) in cases() {
            let response = v2_call(Call::new("tools/call", attribution.clone())).await;
            assert_eq!(response["result"]["result"], seen, "{attribution}");
        }
        let response = v2_call(Call::new("tools/call", json!({"sessionId": 42}))).await;
        assert_eq!(response["error"]["code"], -32602, "{response}");
    }

    fn invalid_params(data: &str) -> Value {
        json!({"error": {"code": -32602, "message": "Invalid params", "data": data}})
    }

    /// Each request and the body of its response: the outer error of a
    /// rejected, failed or cancelled request, or the carrier of a request the
    /// service ran.
    fn rejection_cases() -> Vec<(Call, Value)> {
        let missing = invalid_params("this MCP server requires sessionId on mcp/message");
        let ignored = invalid_params("sessionId rejected: ignored by the service");
        let child = json!({"sessionId": "child"});
        vec![
            // A request without a session is rejected, through the context or
            // through the connection a tool receives.
            (Call::new("test/require", json!({})), missing.clone()),
            (
                Call::new("test/require", json!({"sessionId": null})),
                missing.clone(),
            ),
            (Call::new("test/require_on_connection", json!({})), missing),
            // A request with a session runs.
            (
                Call::new("test/require", child.clone()),
                json!({"result": {"result": {"sessionId": "child"}}}),
            ),
            (
                Call::new("test/require_on_connection", child.clone()),
                json!({"result": {"result": {"sessionId": "child"}}}),
            ),
            // A session that the service's own check refuses.
            (
                Call::new("test/reject", child.clone()),
                invalid_params("sessionId rejected: the session is not active"),
            ),
            // The rejection replaces whatever the service returns.
            (
                Call::new("test/reject_then_result", child.clone()),
                ignored.clone(),
            ),
            (
                Call::new("test/reject_then_mcp_error", child.clone()),
                ignored.clone(),
            ),
            (
                Call::new("test/reject_then_other_error", child.clone()),
                ignored,
            ),
            // The first rejection of a request is the one sent.
            (
                Call::new("test/reject_twice", child.clone()),
                invalid_params("sessionId rejected: first"),
            ),
            // A request the Agent cancelled before the service returned keeps
            // Request cancelled, although a rejection was recorded.
            (
                Call {
                    cancel: true,
                    ..Call::new("test/reject_then_wait_for_cancellation", child.clone())
                },
                json!({"error": {"code": -32800, "message": "Request cancelled"}}),
            ),
            // An error that is not recorded as a rejection is a backend
            // failure, whatever its own code is.
            (
                Call::new("test/fail", child.clone()),
                json!({"error": {
                    "code": -33002,
                    "message": "MCP backend failure",
                    "data": "Internal error: \"database unavailable\"",
                }}),
            ),
            (
                Call::new("test/invalid_params_without_rejection", child),
                json!({"error": {
                    "code": -33002,
                    "message": "MCP backend failure",
                    "data": "Invalid params: \"not recorded as a rejection\"",
                }}),
            ),
        ]
    }

    /// Checks `response` against the body `expected` describes.
    fn assert_body(call: &Call, response: &Value, expected: &Value) {
        for key in ["result", "error"] {
            assert_eq!(
                response.get(key),
                expected.get(key),
                "{}: {response}",
                call.method
            );
        }
    }

    #[tokio::test]
    async fn v1_service_rejects_a_request_for_its_session() {
        for (call, expected) in rejection_cases() {
            assert_body(&call, &v1_call(call.clone()).await, &expected);
        }
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[tokio::test]
    async fn v2_service_rejects_a_request_for_its_session() {
        for (call, expected) in rejection_cases() {
            assert_body(&call, &v2_call(call.clone()).await, &expected);
        }
    }
}
