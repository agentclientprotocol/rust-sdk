#![cfg(feature = "unstable_mcp_over_acp")]

use std::time::Duration;

#[cfg(feature = "unstable_protocol_v2")]
use agent_client_protocol::V2ConnectionTo;
#[cfg(feature = "unstable_protocol_v2")]
use agent_client_protocol::schema::v2;
use agent_client_protocol::{
    Agent, ByteStreams, Client, ConnectTo, ConnectionTo, DynConnectTo, Error, Responder,
    RunWithConnectionTo,
    mcp_server::{McpConnectionTo, McpServer, McpServerConnect},
    role,
    schema::v1,
};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const TIMEOUT: Duration = Duration::from_secs(10);
const CASES: &[(&str, &str)] = &[
    (
        "absent",
        r#"{"code":-32000,"message":"backend failed","extension":{"retry":false}}"#,
    ),
    (
        "null",
        r#"{"code":-32000,"message":"backend failed","data":null,"extension":{"retry":false}}"#,
    ),
    (
        "object",
        r#"{"code":-32000,"message":"backend failed","data":{"cause":"upstream"},"extension":{"retry":false}}"#,
    ),
];

struct WireConnector;

impl McpServerConnect<Agent> for WireConnector {
    fn name(&self) -> String {
        "raw-wire-errors".into()
    }

    fn connect(&self, context: McpConnectionTo<Agent>) -> DynConnectTo<role::mcp::Client> {
        assert!(
            context.request_id().is_some(),
            "expected an ACP MCP request"
        );
        DynConnectTo::new(WireBackend)
    }
}

struct WireBackend;

impl ConnectTo<role::mcp::Client> for WireBackend {
    async fn connect_to(self, client: impl ConnectTo<role::mcp::Server>) -> Result<(), Error> {
        let (sdk_output, peer_input) = duplex(4096);
        let (peer_output, sdk_input) = duplex(4096);
        let transport = ByteStreams::new(sdk_output.compat_write(), sdk_input.compat());
        let peer = async move {
            let mut reader = BufReader::new(peer_input);
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .expect("read MCP wire request");
            let request: Value = serde_json::from_str(&line).expect("valid MCP wire request");
            assert_eq!(request["jsonrpc"], "2.0");
            assert_eq!(
                request["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
                "2026-07-28"
            );
            let id = serde_json::to_string(&request["id"]).unwrap();
            let method = request["method"].as_str().expect("MCP method");
            let response = if method == "success" {
                assert_eq!(
                    request["id"], "next",
                    "backend must see the logical request ID"
                );
                format!(r#"{{"jsonrpc":"2.0","id":{id},"result":null}}"#)
            } else {
                let index = CASES
                    .iter()
                    .position(|(case, _)| case == &method)
                    .expect("known test case");
                assert_eq!(
                    request["id"],
                    format!("wire-{index}"),
                    "backend must see the logical request ID"
                );
                let (_, error) = CASES
                    .iter()
                    .find(|(case, _)| case == &method)
                    .expect("known test case");
                // Literal backend JSON, not ACP Error or RawJsonRpcMessage::response:
                // those typed constructors cannot represent data:null or extension.
                format!(r#"{{"jsonrpc":"2.0","id":{id},"error":{error}}}"#)
            };
            let mut output = peer_output;
            output
                .write_all(response.as_bytes())
                .await
                .expect("write MCP response");
            output.write_all(b"\n").await.expect("frame MCP response");
            output.shutdown().await.expect("close MCP response stream");
        };
        let (result, ()) = tokio::join!(client.connect_to(transport), peer);
        result
    }
}

struct IdleRunner;

impl RunWithConnectionTo<Agent> for IdleRunner {
    async fn run_with_connection_to(self, _connection: ConnectionTo<Agent>) -> Result<(), Error> {
        std::future::pending().await
    }
}

#[cfg(feature = "unstable_protocol_v2")]
fn cwd() -> std::path::PathBuf {
    std::env::current_dir().expect("cwd")
}

fn params() -> Map<String, Value> {
    serde_json::from_value(json!({
        "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {}
        }
    }))
    .unwrap()
}

fn assert_error(error: impl serde::Serialize, case: &str) {
    let value = serde_json::to_value(error).unwrap();
    assert_eq!(value["code"], -32000, "{case}");
    assert_eq!(value["message"], "backend failed", "{case}");
    assert_eq!(value["extension"], json!({"retry": false}), "{case}");
    let object = value.as_object().unwrap();
    match case {
        "absent" => assert!(!object.contains_key("data"), "{value}"),
        "null" => assert_eq!(object.get("data"), Some(&Value::Null)),
        "object" => assert_eq!(object.get("data"), Some(&json!({"cause":"upstream"}))),
        _ => unreachable!(),
    }
}

async fn v1_requests(
    connection: ConnectionTo<Client>,
    server_id: v1::McpServerAcpId,
) -> Result<(), Error> {
    for (index, (case, _)) in CASES.iter().enumerate() {
        let response = connection
            .send_request(
                v1::MessageMcpRequest::new(server_id.clone(), format!("wire-{index}"), *case)
                    .params(params()),
            )
            .block_task()
            .await?;
        match response {
            v1::MessageMcpResponse::Error { error, .. } => assert_error(error, case),
            other => panic!("{case}: expected inner MCP error, got {other:?}"),
        }
    }
    let response = connection
        .send_request(v1::MessageMcpRequest::new(server_id, "next", "success").params(params()))
        .block_task()
        .await?;
    assert!(matches!(
        response,
        v1::MessageMcpResponse::Result {
            result: Value::Null,
            ..
        }
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v1_connector_preserves_backend_wire_errors() {
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let done_tx = std::sync::Mutex::new(Some(done_tx));
    let agent = Agent.builder().on_receive_request(
        async move |request: v1::NewSessionRequest,
                    responder: Responder<v1::NewSessionResponse>,
                    connection: ConnectionTo<Client>| {
            let [v1::McpServer::Acp(server)] = request.mcp_servers.as_slice() else {
                panic!("expected one native MCP server")
            };
            let server_id = server.server_id.clone();
            responder.respond(v1::NewSessionResponse::new(v1::SessionId::new(
                "wire-session",
            )))?;
            let done = done_tx.lock().unwrap().take().expect("one setup request");
            let requests = connection.clone();
            connection.spawn(async move {
                drop(done.send(v1_requests(requests, server_id).await));
                Ok(())
            })
        },
        agent_client_protocol::on_receive_request!(),
    );
    let test = Client.builder().connect_with(agent, async |connection| {
        connection
            .build_session_cwd()?
            .with_mcp_server(McpServer::<Agent, _>::new(WireConnector, IdleRunner))?
            .block_task()
            .run_until(async |_session| {
                done_rx.await.map_err(Error::into_internal_error)??;
                Ok(())
            })
            .await?;
        Ok(())
    });
    tokio::time::timeout(TIMEOUT, test)
        .await
        .expect("v1 connector timed out")
        .expect("v1 connector failed");
}

#[cfg(feature = "unstable_protocol_v2")]
async fn v2_requests(
    connection: V2ConnectionTo<Client>,
    server_id: v2::McpServerAcpId,
) -> Result<(), Error> {
    for (index, (case, _)) in CASES.iter().enumerate() {
        let response = connection
            .send_request(
                v2::MessageMcpRequest::new(server_id.clone(), format!("wire-{index}"), *case)
                    .params(params()),
            )
            .block_task()
            .await?;
        match response {
            v2::MessageMcpResponse::Error { error, .. } => assert_error(error, case),
            other => panic!("{case}: expected inner MCP error, got {other:?}"),
        }
    }
    let response = connection
        .send_request(v2::MessageMcpRequest::new(server_id, "next", "success").params(params()))
        .block_task()
        .await?;
    assert!(matches!(
        response,
        v2::MessageMcpResponse::Result {
            result: Value::Null,
            ..
        }
    ));
    Ok(())
}

#[cfg(feature = "unstable_protocol_v2")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_connector_preserves_backend_wire_errors() {
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let done_tx = std::sync::Mutex::new(Some(done_tx));
    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(v2::InitializeResponse::new(
                    request.protocol_version,
                    v2::Implementation::new("wire-backend-test", env!("CARGO_PKG_VERSION")),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::NewSessionRequest,
                        responder: Responder<v2::NewSessionResponse>,
                        connection: V2ConnectionTo<Client>| {
                let [v2::McpServer::Acp(server)] = request.mcp_servers.as_slice() else {
                    panic!("expected one native MCP server")
                };
                let server_id = server.server_id.clone();
                responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                    "wire-session",
                )))?;
                let done = done_tx.lock().unwrap().take().expect("one setup request");
                let requests = connection.clone();
                connection.spawn(async move {
                    drop(done.send(v2_requests(requests, server_id).await));
                    Ok(())
                })
            },
            agent_client_protocol::on_receive_request!(),
        );
    let test = Client.v2().connect_with(agent, async |connection| {
        connection
            .send_request(v2::InitializeRequest::new(
                agent_client_protocol::schema::ProtocolVersion::V2,
                v2::Implementation::new("wire-backend-test", env!("CARGO_PKG_VERSION")),
            ))
            .block_task()
            .await?;
        let session = connection
            .build_session_from(v2::NewSessionRequest::new(cwd()))
            .with_mcp_server(McpServer::<Agent, _>::new(WireConnector, IdleRunner))?
            .start_session()
            .block_task()
            .await?;
        done_rx.await.map_err(Error::into_internal_error)??;
        drop(session);
        Ok(())
    });
    tokio::time::timeout(TIMEOUT, test)
        .await
        .expect("v2 connector timed out")
        .expect("v2 connector failed");
}
