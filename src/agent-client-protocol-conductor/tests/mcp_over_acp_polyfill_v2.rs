//! End-to-end v2 coverage for the request-scoped MCP HTTP adapter.
//!
//! Also needs the polyfill crate's `unstable_protocol_v2` feature. The test's
//! `required-features` in `Cargo.toml` skip it otherwise.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use agent_client_protocol::v2::ConnectionTo;
use agent_client_protocol::{
    Agent, Client, Conductor, ConnectTo, Proxy, schema::ProtocolVersion, v2::schema as v2,
};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use agent_client_protocol_polyfill::mcp_over_acp::{
    McpHttpRequestAttribution, McpHttpRequestInfo, McpOverAcpPolyfill,
};
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

/// The `sessionId` and `toolCallId` of one `mcp/message` request.
type Attribution = (Option<String>, Option<String>);

/// Records the logical IDs, the number of requests, the number of cancelled
/// requests, and the attribution of each request.
struct TestProvider(
    Arc<Mutex<Vec<String>>>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<Attribution>>>,
);

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
                    let metadata = &request.params.as_ref().expect("request metadata")["_meta"];
                    assert_eq!(
                        metadata["io.modelcontextprotocol/protocolVersion"],
                        "2026-07-28"
                    );
                    assert!(metadata["io.modelcontextprotocol/clientCapabilities"].is_object());
                    assert!(uuid::Uuid::parse_str(&request.request_id.to_string()).is_ok());
                    self.0.lock().unwrap().push(request.request_id.to_string());
                    self.1.fetch_add(1, Ordering::SeqCst);
                    self.3.lock().unwrap().push((
                        request.session_id.as_ref().map(ToString::to_string),
                        request.tool_call_id.as_ref().map(ToString::to_string),
                    ));
                    if request.method == "subscriptions/listen"
                        || request.method == "subscriptions/flood"
                    {
                        let params = if request.method == "subscriptions/flood" {
                            serde_json::Map::from_iter([(
                                "payload".into(),
                                serde_json::json!("x".repeat(300 * 1024)),
                            )])
                        } else {
                            assert_eq!(
                                request.params.as_ref().unwrap()["notifications"],
                                serde_json::json!({"toolsListChanged":true})
                            );
                            serde_json::json!({
                                "_meta":{"io.modelcontextprotocol/subscriptionId":
                                    request.request_id.to_string(), "fixture":"preserved"},
                                "notifications":{"toolsListChanged":true}
                            })
                            .as_object()
                            .unwrap()
                            .clone()
                        };
                        cx.send_notification_to(
                            Agent,
                            v2::MessageMcpNotification::new(
                                SERVER_ID,
                                request.request_id.clone(),
                                "notifications/subscriptions/acknowledged",
                            )
                            .params(params),
                        )?;
                        if request.method == "subscriptions/listen" {
                            cx.send_notification_to(
                                Agent,
                                v2::MessageMcpNotification::new(
                                    SERVER_ID,
                                    request.request_id.clone(),
                                    "notifications/tools/list_changed",
                                )
                                .params(
                                    serde_json::json!({
                                        "_meta":{"io.modelcontextprotocol/subscriptionId":
                                            request.request_id.to_string(), "fixture":"preserved"}
                                    })
                                    .as_object()
                                    .unwrap()
                                    .clone(),
                                ),
                            )?;
                        }
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
                        "tools/list" => serde_json::json!({"resultType":"complete","tools":[
                            {"name":"ping","inputSchema":{"type":"object","properties":{}}},
                            {"name":"restricted","inputSchema":{"type":"object","properties":{
                                "region":{"type":"string","x-mcp-header":"Region"}
                            }}}
                        ]}),
                        "tools/call" => serde_json::json!({"resultType":"complete","content":[]}),
                        "tools/error" => {
                            return responder.respond(serde_json::from_value::<
                                v2::MessageMcpResponse,
                            >(
                                serde_json::json!({"error":{"code":-32000,"message":"peer-owned",
                                    "data":{"source":"backend"},"extension":{"preserve":true}}}),
                            )?);
                        }
                        _ => {
                            return responder.respond_with_error(
                                agent_client_protocol::Error::method_not_found(),
                            );
                        }
                    };
                    responder.respond(serde_json::from_value::<v2::MessageMcpResponse>(
                        serde_json::json!({"result":result}),
                    )?)
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
    editor: impl AsyncFnOnce(ConnectionTo<Agent>) -> Result<(), agent_client_protocol::Error>,
) -> Result<(), agent_client_protocol::Error> {
    run_chain(
        TestAgent {
            capabilities,
            observed,
        },
        TestProvider(ids, count, cancelled, Arc::default()),
        McpOverAcpPolyfill::http(),
        editor,
    )
    .await
}

async fn run_chain(
    agent: TestAgent,
    provider: TestProvider,
    polyfill: McpOverAcpPolyfill,
    editor: impl AsyncFnOnce(ConnectionTo<Agent>) -> Result<(), agent_client_protocol::Error>,
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
                ProxiesAndAgent::new(agent).proxy(provider).proxy(polyfill),
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
    post_with_meta(url, bearer, method, tool, serde_json::Map::new()).await
}

/// Like [`post`], with `host_meta` added to the request's `_meta`, as the
/// host of an MCP client would.
async fn post_with_meta(
    url: &str,
    bearer: &str,
    method: &str,
    tool: &str,
    host_meta: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    let (address, route) = url
        .strip_prefix("http://")
        .unwrap()
        .split_once('/')
        .unwrap();
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut params = serde_json::json!({
        "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
            "io.modelcontextprotocol/clientCapabilities":{},
            "io.modelcontextprotocol/clientInfo":{"name":"http-fixture","version":"1"}}
    });
    params["_meta"].as_object_mut().unwrap().extend(host_meta);
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
        "POST /{route} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nAuthorization: {bearer}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: {method}\r\n{name}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        stream.read_to_string(&mut response),
    )
    .await
    .expect("HTTP response should finish")
    .unwrap();
    // Do not echo peer-controlled response bodies into CI failure logs.
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected HTTP status 200"
    );
    serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap()
}

async fn assert_subscription_events(stream: &mut tokio::net::TcpStream) {
    let events = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let mut output = String::new();
        loop {
            let mut buf = [0; 2048];
            let n = stream.read(&mut buf).await.unwrap();
            assert_ne!(n, 0, "subscription ended before fixture events");
            output.push_str(std::str::from_utf8(&buf[..n]).unwrap());
            let events = output
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter_map(|json| serde_json::from_str::<serde_json::Value>(json).ok())
                .collect::<Vec<_>>();
            if events.len() == 2 {
                assert!(
                    output.starts_with("HTTP/1.1 200"),
                    "expected HTTP status 200"
                );
                return events;
            }
        }
    })
    .await
    .expect("subscription must acknowledge its filter and deliver the matching event");
    assert_eq!(
        events[0]["method"],
        "notifications/subscriptions/acknowledged"
    );
    assert_eq!(
        events[0]["params"]["notifications"],
        serde_json::json!({"toolsListChanged":true})
    );
    assert_eq!(events[1]["method"], "notifications/tools/list_changed");
    for event in events {
        assert_eq!(
            event["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
            73
        );
        assert_eq!(event["params"]["_meta"]["fixture"], "preserved");
    }
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
            // A direct call does not require discovery or an internal tools/list lookup.
            let direct = post(&url, &bearer, "tools/call", "ping").await;
            assert_eq!(
                direct,
                serde_json::json!({"jsonrpc":"2.0","id":"same","result":{"resultType":"complete","content":[]}})
            );
            let annotated = post(&url, &bearer, "tools/call", "restricted").await;
            assert_eq!(annotated["result"], serde_json::json!({"resultType":"complete","content":[]}));
            let peer_error = post(&url, &bearer, "tools/error", "").await;
            assert_eq!(
                peer_error["error"],
                serde_json::json!({"code":-32000,
                "message":"peer-owned","data":{"source":"backend"},"extension":{"preserve":true}})
            );
            let (a, b) = tokio::join!(
                post(&url, &bearer, "tools/list", ""),
                post(&url, &bearer, "tools/list", "")
            );
            assert_eq!(
                a,
                serde_json::json!({"jsonrpc":"2.0","id":"same","result":{"resultType":"complete","tools":[
                    {"name":"ping","inputSchema":{"type":"object","properties":{}}},
                    {"name":"restricted","inputSchema":{"type":"object","properties":{
                        "region":{"type":"string"}
                    }}}
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
    assert_eq!(
        ids.iter().collect::<std::collections::HashSet<_>>().len(),
        5,
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
            let (address, route) = url.strip_prefix("http://").unwrap().split_once('/').unwrap();
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            let body = serde_json::json!({
                "jsonrpc":"2.0","id":73,"method":"subscriptions/listen",
                "params":{"notifications":{"toolsListChanged":true}, "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities":{}}}
            }).to_string();
            let request = format!(
                "POST /{route} HTTP/1.1\r\nHost: {address}\r\nAuthorization: {bearer}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: subscriptions/listen\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(request.as_bytes()).await.unwrap();
            assert_subscription_events(&mut stream).await;
            // A second live POST uses the same external ID, but must retain its
            // own generated logical ID and cancellation lifetime.
            let mut second = tokio::net::TcpStream::connect(address).await.unwrap();
            second.write_all(request.as_bytes()).await.unwrap();
            assert_subscription_events(&mut second).await;
            let logical_ids = ids.lock().unwrap().clone();
            assert_eq!(logical_ids.len(), 2);
            assert_ne!(logical_ids[0], logical_ids[1]);
            drop(stream);
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while cancelled.load(Ordering::SeqCst) != 1 {
                    tokio::task::yield_now().await;
                }
            }).await.expect("closing the HTTP stream must cancel native ACP request");
            assert_eq!(post(&url, &bearer, "tools/call", "ping").await["result"]["content"], serde_json::json!([]));
            assert_eq!(cancelled.load(Ordering::SeqCst), 1, "the sibling subscription remains active");
            drop(second);
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while cancelled.load(Ordering::SeqCst) != 2 {
                    tokio::task::yield_now().await;
                }
            }).await.expect("closing the second stream must cancel its own ACP request");
            let overflow = post(&url, &bearer, "subscriptions/flood", "").await;
            assert_eq!(overflow["error"]["code"], -33000, "{overflow}");
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

/// What a host that recorded two tool calls returns for `request`. It finds
/// the tool call by the tool-use ID its MCP client sends in `_meta`.
fn recorded_attribution(request: &McpHttpRequestInfo) -> Option<McpHttpRequestAttribution> {
    let meta = request.params.as_ref()?.get("_meta")?;
    match meta.get("example.com/toolUseId")?.as_str()? {
        "toolu_1" => Some(McpHttpRequestAttribution::new("sess_child_1").tool_call_id("call_001")),
        "toolu_2" => Some(McpHttpRequestAttribution::new("sess_root")),
        _ => None,
    }
}

/// The bridge sends the session and tool call that the host's resolver
/// returns. It sends neither for a request the host cannot match, and for
/// every request when there is no resolver.
#[tokio::test]
async fn v2_http_requests_carry_only_the_attribution_the_host_supplies()
-> Result<(), agent_client_protocol::Error> {
    for with_resolver in [true, false] {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let attributions = Arc::new(Mutex::new(Vec::new()));
        let asked = Arc::new(Mutex::new(Vec::new()));
        let polyfill = if with_resolver {
            let asked = asked.clone();
            McpOverAcpPolyfill::http().with_attribution(move |request| {
                let attribution = recorded_attribution(&request);
                asked.lock().unwrap().push(request);
                // A resolver can wait, for example for the host's own record
                // of the tool call.
                async move {
                    tokio::task::yield_now().await;
                    Ok(attribution)
                }
            })
        } else {
            McpOverAcpPolyfill::http()
        };
        run_chain(
            TestAgent {
                capabilities: v2::AgentCapabilities::new().session(
                    v2::SessionCapabilities::new()
                        .mcp(v2::McpCapabilities::new().http(v2::McpHttpCapabilities::new())),
                ),
                observed: observed.clone(),
            },
            TestProvider(
                Arc::default(),
                Arc::default(),
                Arc::default(),
                attributions.clone(),
            ),
            polyfill,
            async |connection| {
                connection.send_request(initialize()).block_task().await?;
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
                    (server.url.clone(), server.headers[0].value.clone())
                };
                for tool_use in ["toolu_1", "toolu_2", "toolu_unknown"] {
                    let host_meta = serde_json::Map::from_iter([(
                        "example.com/toolUseId".to_string(),
                        serde_json::json!(tool_use),
                    )]);
                    let call = post_with_meta(&url, &bearer, "tools/call", "ping", host_meta).await;
                    assert!(call["result"].is_object(), "{tool_use}");
                }
                Ok(())
            },
        )
        .await?;

        let expected = if with_resolver {
            vec![
                (
                    Some("sess_child_1".to_string()),
                    Some("call_001".to_string()),
                ),
                (Some("sess_root".to_string()), None),
                (None, None),
            ]
        } else {
            vec![(None, None); 3]
        };
        assert_eq!(*attributions.lock().unwrap(), expected);

        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), if with_resolver { 3 } else { 0 });
        if let Some(first) = asked.first() {
            assert_eq!(first.server_id, SERVER_ID);
            assert_eq!(first.method, "tools/call");
            let params = first.params.as_ref().expect("inner params");
            assert_eq!(params["name"], "ping");
            assert_eq!(params["_meta"]["example.com/toolUseId"], "toolu_1");
        }
    }
    Ok(())
}
