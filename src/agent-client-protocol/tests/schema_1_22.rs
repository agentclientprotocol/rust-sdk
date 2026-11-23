//! Checks the changes from schema 1.22. Two malformed v1 payloads get Invalid
//! params (`-32602`) instead of being read as something else, and the session
//! and tool call of an `mcp/message` request reach the MCP service.

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
        Agent, Channel, Client, Error, NullRun,
        mcp_server::{McpOutcome, McpRequest, McpRequestContext, McpServer, McpService},
    };
    #[cfg(feature = "unstable_protocol_v2")]
    use agent_client_protocol::{schema::ProtocolVersion, v2::schema as v2};
    use futures::future::BoxFuture;
    use serde_json::{Value, json};
    use tokio::sync::oneshot;

    /// Answers every request with the session and tool call it was sent for.
    struct ReportAttribution;

    impl McpService<Agent> for ReportAttribution {
        fn execute(
            &self,
            _request: McpRequest,
            context: McpRequestContext<Agent>,
        ) -> BoxFuture<'static, Result<McpOutcome, Error>> {
            // A `tool_fn` closure receives `context.connection()`, so check
            // that it carries the same values.
            let connection = context.connection();
            assert_eq!(context.session_id(), connection.session_id());
            assert_eq!(context.tool_call_id(), connection.tool_call_id());
            let seen = json!({
                "sessionId": context.session_id().map(|id| id.0.to_string()),
                "toolCallId": context.tool_call_id().map(|id| id.0.to_string()),
            });
            Box::pin(async move { Ok(McpOutcome::Result(seen)) })
        }
    }

    fn server() -> McpServer<Agent, NullRun> {
        McpServer::new_service("attribution", ReportAttribution, NullRun)
    }

    /// Plays the Agent on `peer`: answers the session setup request, sends one
    /// `mcp/message` request with `attribution` merged into its params, and
    /// returns the response.
    async fn call(peer: &mut Channel, attribution: &Value) -> Value {
        let setup = receive(peer).await;
        let server_id = setup["params"]["mcpServers"][0]["serverId"].clone();
        send(
            peer,
            json!({"jsonrpc": "2.0", "id": setup["id"], "result": {"sessionId": "parent"}}),
        );
        let mut params = json!({
            "serverId": server_id,
            "requestId": "call",
            "method": "tools/call",
            "params": {"_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {}
            }}
        });
        params
            .as_object_mut()
            .unwrap()
            .extend(attribution.as_object().unwrap().clone());
        send(
            peer,
            json!({"jsonrpc": "2.0", "id": "call", "method": "mcp/message", "params": params}),
        );
        let response = receive(peer).await;
        assert_eq!(response["id"], "call", "{response}");
        response
    }

    async fn v1_call(attribution: Value) -> Value {
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
            let response = call(&mut peer, &attribution).await;
            stop_tx.send(()).unwrap();
            client.await.unwrap().unwrap();
            response
        })
        .await
        .expect("the v1 MCP call timed out")
    }

    #[cfg(feature = "unstable_protocol_v2")]
    async fn v2_call(attribution: Value) -> Value {
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
            let response = call(&mut peer, &attribution).await;
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
            let response = v1_call(attribution.clone()).await;
            assert_eq!(response["result"]["result"], seen, "{attribution}");
        }
        // A `sessionId` that is not a string is a malformed envelope.
        let response = v1_call(json!({"sessionId": 42})).await;
        assert_eq!(response["error"]["code"], -32602, "{response}");
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[tokio::test]
    async fn v2_service_sees_the_session_and_tool_call_of_a_request() {
        for (attribution, seen) in cases() {
            let response = v2_call(attribution.clone()).await;
            assert_eq!(response["result"]["result"], seen, "{attribution}");
        }
        let response = v2_call(json!({"sessionId": 42})).await;
        assert_eq!(response["error"]["code"], -32602, "{response}");
    }
}
