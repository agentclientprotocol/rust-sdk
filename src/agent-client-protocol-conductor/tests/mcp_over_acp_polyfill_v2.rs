#![cfg(feature = "unstable_protocol_v2")]

//! End-to-end v2 coverage for the request-scoped MCP HTTP adapter.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use agent_client_protocol::{
    Agent, Client, Conductor, ConnectTo, Proxy, V2ConnectionTo,
    schema::{ProtocolVersion, v2},
};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use agent_client_protocol_polyfill::mcp_over_acp::McpOverAcpPolyfill;
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const SERVER_ID: &str = "v2-server-id";

struct TestAgent {
    capabilities: v2::AgentCapabilities,
    observed: Arc<Mutex<Vec<v2::McpServer>>>,
}

impl ConnectTo<Client> for TestAgent {
    async fn connect_to(
        self,
        client: impl ConnectTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error> {
        let capabilities = self.capabilities;
        let observed = self.observed;
        Agent
            .v2()
            .name("v2-http-test-agent")
            .on_receive_request(
                async move |request: v2::InitializeRequest, responder, _cx| {
                    responder.respond(
                        v2::InitializeResponse::new(
                            request.protocol_version,
                            v2::Implementation::new("test", "1.0.0"),
                        )
                        .capabilities(capabilities.clone()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: v2::NewSessionRequest, responder, _cx| {
                    *observed.lock().unwrap() = request.mcp_servers;
                    responder.respond(v2::NewSessionResponse::new("session"))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}

struct TestProvider(Arc<Mutex<Vec<String>>>, Arc<AtomicUsize>, Arc<AtomicUsize>);

impl ConnectTo<Conductor> for TestProvider {
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol::Error> {
        Proxy
            .v2()
            .name("v2-mcp-provider")
            .on_receive_request_from(
                Agent,
                async move |request: v2::MessageMcpRequest, responder, cx| {
                    assert_eq!(request.server_id.to_string(), SERVER_ID);
                    self.0.lock().unwrap().push(request.request_id.to_string());
                    self.1.fetch_add(1, Ordering::SeqCst);
                    if request.method == "subscriptions/listen"
                        || request.method == "subscriptions/flood"
                    {
                        let params = if request.method == "subscriptions/flood" {
                            serde_json::Map::from_iter([(
                                "payload".into(),
                                serde_json::json!("x".repeat(300 * 1024)),
                            )])
                        } else {
                            serde_json::Map::from_iter([(
                                "_meta".into(),
                                serde_json::json!({"io.modelcontextprotocol/subscriptionId":
                                    request.request_id.to_string()}),
                            )])
                        };
                        cx.send_notification_to(
                            Agent,
                            v2::MessageMcpNotification::new(
                                SERVER_ID,
                                request.request_id,
                                "notifications/subscriptions/acknowledged",
                            )
                            .params(params),
                        )?;
                        let cancelled = responder.cancellation();
                        let count = self.2.clone();
                        cx.spawn(async move {
                            cancelled.cancelled().await;
                            count.fetch_add(1, Ordering::SeqCst);
                            responder.respond_with_error(
                                agent_client_protocol::Error::request_cancelled(),
                            )
                        })?;
                        return Ok(());
                    }
                    let result = match request.method.as_str() {
                        "tools/list" => serde_json::json!({"tools":[
                            {"name":"ping","inputSchema":{"type":"object","properties":{}}},
                            {"name":"restricted","inputSchema":{"type":"object","properties":{
                                "region":{"type":"string","x-mcp-header":"Region"}
                            }}}
                        ]}),
                        "tools/call" => serde_json::json!({"content":[]}),
                        _ => {
                            return responder.respond_with_error(
                                agent_client_protocol::Error::method_not_found(),
                            );
                        }
                    };
                    responder.respond(serde_json::from_value::<v2::MessageMcpResponse>(result)?)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}

async fn run(
    capabilities: v2::AgentCapabilities,
    observed: Arc<Mutex<Vec<v2::McpServer>>>,
    ids: Arc<Mutex<Vec<String>>>,
    count: Arc<AtomicUsize>,
    cancelled: Arc<AtomicUsize>,
    editor: impl AsyncFnOnce(V2ConnectionTo<Agent>) -> Result<(), agent_client_protocol::Error>,
) -> Result<(), agent_client_protocol::Error> {
    let (editor_out, conductor_in) = duplex(4096);
    let (conductor_out, editor_in) = duplex(4096);
    let transport =
        agent_client_protocol::ByteStreams::new(editor_out.compat_write(), editor_in.compat());
    Client
        .v2()
        .name("v2-mcp-test-client")
        .with_spawned(|_cx| async move {
            ConductorImpl::new_agent(
                "v2-mcp-test-conductor",
                ProxiesAndAgent::new(TestAgent {
                    capabilities,
                    observed,
                })
                .proxy(TestProvider(ids, count, cancelled))
                .proxy(McpOverAcpPolyfill::http()),
            )
            .run(agent_client_protocol::ByteStreams::new(
                conductor_out.compat_write(),
                conductor_in.compat(),
            ))
            .await
        })
        .connect_with(transport, editor)
        .await
}

fn native_server() -> v2::McpServer {
    let mut meta = v2::Meta::new();
    meta.insert("preserve".into(), serde_json::json!(true));
    v2::McpServer::Acp(v2::McpServerAcp::new("native", SERVER_ID).meta(meta))
}

fn initialize() -> v2::InitializeRequest {
    v2::InitializeRequest::new(
        ProtocolVersion::V2,
        v2::Implementation::new("test", "1.0.0"),
    )
}

async fn post(url: &str, bearer: &str, method: &str, tool: &str) -> serde_json::Value {
    let address = url.strip_prefix("http://").unwrap();
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut params = serde_json::json!({
        "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}
    });
    if method == "tools/call" {
        params["name"] = serde_json::json!(tool);
        params["arguments"] = serde_json::json!({});
    }
    let body = serde_json::json!({"jsonrpc":"2.0","id":"same","method":method,
        "params":params})
    .to_string();
    let name = if method == "tools/call" {
        format!("Mcp-Name: {tool}\r\n")
    } else {
        String::new()
    };
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nAuthorization: {bearer}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: {method}\r\n{name}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap()
}

#[tokio::test]
async fn modern_http_v2_requests_are_stateless_and_isolated()
-> Result<(), agent_client_protocol::Error> {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let ids = Arc::new(Mutex::new(Vec::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let caps = v2::AgentCapabilities::new().session(
        v2::SessionCapabilities::new()
            .mcp(v2::McpCapabilities::new().http(v2::McpHttpCapabilities::new())),
    );
    run(
        caps,
        observed.clone(),
        ids.clone(),
        count.clone(),
        Arc::default(),
        async |connection| {
            let initialized = connection.send_request(initialize()).block_task().await?;
            assert!(
                initialized
                    .capabilities
                    .session
                    .unwrap()
                    .mcp
                    .unwrap()
                    .acp
                    .is_some()
            );
            connection
                .send_request(
                    v2::NewSessionRequest::new(PathBuf::from("/tmp"))
                        .mcp_servers(vec![native_server()]),
                )
                .block_task()
                .await?;
            let (url, bearer) = {
                let observed = observed.lock().unwrap();
                let v2::McpServer::Http(server) = &observed[0] else {
                    panic!("expected HTTP endpoint")
                };
                assert_eq!(
                    server.meta.as_ref().unwrap().get("preserve"),
                    Some(&serde_json::json!(true))
                );
                assert_eq!(server.headers[0].name, "Authorization");
                (server.url.clone(), server.headers[0].value.clone())
            };
            // No prior client tools/list: the adapter looks up the descriptor
            // internally, rejecting annotated tools instead of skipping mirrors.
            let direct = post(&url, &bearer, "tools/call", "ping").await;
            assert_eq!(
                direct,
                serde_json::json!({"jsonrpc":"2.0","id":"same","result":{"content":[]}})
            );
            let annotated = post(&url, &bearer, "tools/call", "restricted").await;
            assert_eq!(annotated["error"]["code"], -32602);
            let (a, b) = tokio::join!(
                post(&url, &bearer, "tools/list", ""),
                post(&url, &bearer, "tools/list", "")
            );
            assert_eq!(
                a,
                serde_json::json!({"jsonrpc":"2.0","id":"same","result":{"tools":[
                    {"name":"ping","inputSchema":{"type":"object","properties":{}}}
                ]}})
            );
            assert_eq!(a, b);
            Ok(())
        },
    )
    .await?;
    assert_eq!(count.load(Ordering::SeqCst), 5);
    let ids = ids.lock().unwrap();
    assert_eq!(ids.len(), 5);
    assert_ne!(
        ids[0], ids[1],
        "external JSON-RPC IDs must not collide at the ACP hop"
    );
    Ok(())
}

#[tokio::test]
async fn native_v2_declarations_pass_through_without_http()
-> Result<(), agent_client_protocol::Error> {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let caps = v2::AgentCapabilities::new().session(
        v2::SessionCapabilities::new()
            .mcp(v2::McpCapabilities::new().acp(v2::McpAcpCapabilities::new())),
    );
    run(
        caps,
        observed.clone(),
        Arc::default(),
        Arc::default(),
        Arc::default(),
        async |connection| {
            let initialized = connection.send_request(initialize()).block_task().await?;
            assert!(
                initialized
                    .capabilities
                    .session
                    .unwrap()
                    .mcp
                    .unwrap()
                    .acp
                    .is_some()
            );
            connection
                .send_request(
                    v2::NewSessionRequest::new(PathBuf::from("/tmp"))
                        .mcp_servers(vec![native_server()]),
                )
                .block_task()
                .await?;
            Ok(())
        },
    )
    .await?;
    assert_eq!(*observed.lock().unwrap(), vec![native_server()]);
    Ok(())
}

#[tokio::test]
async fn closing_subscription_stream_cancels_only_its_native_request()
-> Result<(), agent_client_protocol::Error> {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let cancelled = Arc::new(AtomicUsize::new(0));
    let ids = Arc::new(Mutex::new(Vec::new()));
    let caps = v2::AgentCapabilities::new().session(
        v2::SessionCapabilities::new()
            .mcp(v2::McpCapabilities::new().http(v2::McpHttpCapabilities::new())),
    );
    run(
        caps, observed.clone(), ids.clone(), Arc::default(), cancelled.clone(),
        async |connection| {
            connection.send_request(initialize()).block_task().await?;
            connection.send_request(v2::NewSessionRequest::new(PathBuf::from("/tmp"))
                .mcp_servers(vec![native_server()])).block_task().await?;
            let (url, bearer) = {
                let observed = observed.lock().unwrap();
                let v2::McpServer::Http(server) = &observed[0] else { panic!("expected HTTP endpoint") };
                (server.url.clone(), server.headers[0].value.clone())
            };
            let address = url.strip_prefix("http://").unwrap();
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            let body = serde_json::json!({
                "jsonrpc":"2.0","id":73,"method":"subscriptions/listen",
                "params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}
            }).to_string();
            let request = format!(
                "POST / HTTP/1.1\r\nHost: {address}\r\nAuthorization: {bearer}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: subscriptions/listen\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut output = String::new();
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while !output.contains("notifications/subscriptions/acknowledged") {
                    let mut buf = [0; 2048];
                    let n = stream.read(&mut buf).await.unwrap();
                    assert_ne!(n, 0, "subscription stream closed before ack");
                    output.push_str(std::str::from_utf8(&buf[..n]).unwrap());
                }
            }).await.expect("expected subscription acknowledgment");
            assert!(output.contains("\"io.modelcontextprotocol/subscriptionId\":73"), "{output}");
            // A second live POST uses the same external ID, but must retain its
            // own generated logical ID and cancellation lifetime.
            let mut second = tokio::net::TcpStream::connect(address).await.unwrap();
            second.write_all(request.as_bytes()).await.unwrap();
            let mut second_output = String::new();
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while !second_output.contains("notifications/subscriptions/acknowledged") {
                    let mut buf = [0; 2048];
                    let n = second.read(&mut buf).await.unwrap();
                    assert_ne!(n, 0, "second subscription closed before ack");
                    second_output.push_str(std::str::from_utf8(&buf[..n]).unwrap());
                }
            }).await.expect("expected second subscription acknowledgment");
            assert!(second_output.contains("\"io.modelcontextprotocol/subscriptionId\":73"), "{second_output}");
            let logical_ids = ids.lock().unwrap().clone();
            assert_eq!(logical_ids.len(), 2);
            assert_ne!(logical_ids[0], logical_ids[1]);
            drop(stream);
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while cancelled.load(Ordering::SeqCst) != 1 {
                    tokio::task::yield_now().await;
                }
            }).await.expect("closing the HTTP stream must cancel native ACP request");
            drop(second);
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while cancelled.load(Ordering::SeqCst) != 2 {
                    tokio::task::yield_now().await;
                }
            }).await.expect("closing the second stream must cancel its own ACP request");
            let overflow = post(&url, &bearer, "subscriptions/flood", "").await;
            assert_eq!(overflow["error"]["code"], -32000, "{overflow}");
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while cancelled.load(Ordering::SeqCst) != 3 {
                    tokio::task::yield_now().await;
                }
            }).await.expect("overflow must cancel only its native ACP request");
            assert_eq!(post(&url, &bearer, "tools/list", "").await["result"]["tools"][0]["name"], "ping");
            Ok(())
        },
    ).await
}
