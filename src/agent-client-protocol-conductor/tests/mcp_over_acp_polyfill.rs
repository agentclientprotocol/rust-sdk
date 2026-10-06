//! Integration tests for the public MCP-over-ACP compatibility proxy.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, InitializeRequest, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, McpCapabilities, McpServer, McpServerAcp, MessageMcpNotification,
    MessageMcpRequest, MessageMcpResponse, NewSessionRequest, NewSessionResponse,
    ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities, SessionResumeCapabilities,
};
use agent_client_protocol::{Agent, Client, Conductor, ConnectTo, Proxy};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use agent_client_protocol_polyfill::mcp_over_acp::McpOverAcpPolyfill;
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const SERVER_NAME: &str = "shared-server";
const SERVER_ID: &str = "shared-server-id";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetupMethod {
    New,
    Load,
    Resume,
}

#[derive(Debug)]
struct SetupRequest {
    method: SetupMethod,
    mcp_servers: Vec<McpServer>,
}

#[derive(Default)]
struct ObservedRequests {
    setup: Mutex<Vec<SetupRequest>>,
}

impl ObservedRequests {
    fn record(&self, method: SetupMethod, mcp_servers: Vec<McpServer>) {
        self.setup
            .lock()
            .expect("setup request mutex should not be poisoned")
            .push(SetupRequest {
                method,
                mcp_servers,
            });
    }
}

struct RecordingAgent {
    capabilities: AgentCapabilities,
    observed: Arc<ObservedRequests>,
}

#[derive(Default)]
struct NativeMcpProvider {
    request_count: Arc<AtomicUsize>,
    request_ids: Arc<Mutex<Vec<String>>>,
    cancelled_ids: Arc<Mutex<Vec<String>>>,
}

impl ConnectTo<Conductor> for NativeMcpProvider {
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol::Error> {
        Proxy
            .builder()
            .name("native-mcp-provider")
            .on_receive_request_from(
                Agent,
                async move |request: MessageMcpRequest, responder, cx| {
                    assert_eq!(request.server_id.to_string(), SERVER_ID);
                    let params = request.params.as_ref().expect("request metadata");
                    assert_eq!(params["_meta"]["io.modelcontextprotocol/protocolVersion"], "2026-07-28");
                    assert!(params["_meta"]["io.modelcontextprotocol/clientCapabilities"].is_object());
                    let request_id = request.request_id.to_string();
                    assert!(uuid::Uuid::parse_str(&request_id).is_ok());
                    self.request_ids.lock().unwrap().push(request_id.clone());
                    self.request_count.fetch_add(1, Ordering::SeqCst);
                    if request.method == "subscriptions/listen" {
                        assert_eq!(params["notifications"], serde_json::json!({"toolsListChanged":true}));
                        let meta = serde_json::json!({
                            "io.modelcontextprotocol/subscriptionId":request_id,
                            "fixture":"preserved"
                        });
                        for (method, params) in [
                            ("notifications/subscriptions/acknowledged", serde_json::json!({
                                "_meta":meta, "notifications":{"toolsListChanged":true}
                            })),
                            ("notifications/tools/list_changed", serde_json::json!({"_meta":meta})),
                        ] {
                            cx.send_notification_to(Agent, MessageMcpNotification::new(
                                SERVER_ID, request.request_id.clone(), method,
                            ).params(params.as_object().unwrap().clone()))?;
                        }
                        let cancellation = responder.cancellation();
                        let cancelled_ids = self.cancelled_ids.clone();
                        cx.spawn(async move {
                            cancellation.cancelled().await;
                            cancelled_ids.lock().unwrap().push(request_id);
                            responder.respond_with_error(agent_client_protocol::Error::request_cancelled())
                        })?;
                        return Ok(());
                    }
                    let result = match request.method.as_str() {
                        "tools/list" => serde_json::json!({"resultType":"complete","tools":[]}),
                        "tools/call" => {
                            assert_eq!(params["name"], "ping");
                            assert_eq!(params["arguments"], serde_json::json!({"text":"hello"}));
                            assert_eq!(params["_meta"]["progressToken"], "unchanged");
                            serde_json::json!({"resultType":"complete","content":[],"_meta":{"fixture":"preserved"}})
                        }
                        "tools/error" => {
                            return responder.respond(serde_json::from_value::<MessageMcpResponse>(
                                serde_json::json!({"error":{"code":-33001,"message":"peer-owned",
                                    "data":null,"extension":{"preserve":true}}}),
                            )?);
                        }
                        "tools/backend-failure" => {
                            return responder.respond_with_error(agent_client_protocol::Error::internal_error());
                        }
                        _ => panic!("unexpected fixture method {}", request.method),
                    };
                    responder.respond(serde_json::from_value::<MessageMcpResponse>(
                        serde_json::json!({"result":result}),
                    )?)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}

impl ConnectTo<Client> for RecordingAgent {
    async fn connect_to(
        self,
        client: impl ConnectTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error> {
        let capabilities = self.capabilities;
        let new_observed = self.observed.clone();
        let load_observed = self.observed.clone();
        let resume_observed = self.observed;

        Agent
            .builder()
            .name("recording-agent")
            .on_receive_request(
                async move |request: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(request.protocol_version)
                            .agent_capabilities(capabilities.clone()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: NewSessionRequest, responder, _cx| {
                    new_observed.record(SetupMethod::New, request.mcp_servers);
                    responder.respond(NewSessionResponse::new("session-id"))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: LoadSessionRequest, responder, _cx| {
                    load_observed.record(SetupMethod::Load, request.mcp_servers);
                    responder.respond(LoadSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: ResumeSessionRequest, responder, _cx| {
                    resume_observed.record(SetupMethod::Resume, request.mcp_servers);
                    responder.respond(ResumeSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}

fn agent_capabilities(mcp_capabilities: McpCapabilities) -> AgentCapabilities {
    AgentCapabilities::new()
        .load_session(true)
        .session_capabilities(SessionCapabilities::new().resume(SessionResumeCapabilities::new()))
        .mcp_capabilities(mcp_capabilities)
}

fn native_server() -> McpServer {
    let meta = serde_json::Map::from_iter([(
        "source".to_string(),
        serde_json::Value::String("integration-test".to_string()),
    )]);
    McpServer::Acp(McpServerAcp::new(SERVER_NAME, SERVER_ID).meta(meta))
}

async fn http_post(url: &str, bearer: &str, id: i64) -> serde_json::Value {
    post_json(
        url,
        bearer,
        serde_json::json!(id),
        "tools/list",
        serde_json::json!({}),
    )
    .await
}

async fn open_post(
    url: &str,
    bearer: &str,
    id: serde_json::Value,
    method: &str,
    mut params: serde_json::Value,
) -> tokio::net::TcpStream {
    let (address, route) = url
        .strip_prefix("http://")
        .unwrap()
        .split_once('/')
        .unwrap();
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    params["_meta"] = serde_json::json!({
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{},
        "io.modelcontextprotocol/clientInfo":{"name":"http-fixture","version":"1"},
        "progressToken":"unchanged"
    });
    let body =
        serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string();
    let name = params
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(|name| format!("Mcp-Name: {name}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST /{route} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nAuthorization: {bearer}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: {method}\r\n{name}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    stream
}

async fn post_json(
    url: &str,
    bearer: &str,
    id: serde_json::Value,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let mut stream = open_post(url, bearer, id, method, params).await;
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

async fn subscription_events(stream: &mut tokio::net::TcpStream) -> Vec<serde_json::Value> {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
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
    .expect("subscription must acknowledge its filter and deliver the matching event")
}

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

async fn run_with_polyfill(
    agent: RecordingAgent,
    provider_request_count: Arc<AtomicUsize>,
    editor_task: impl AsyncFnOnce(
        agent_client_protocol::ConnectionTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error>,
) -> Result<(), agent_client_protocol::Error> {
    run_with_provider(
        agent,
        NativeMcpProvider {
            request_count: provider_request_count,
            ..NativeMcpProvider::default()
        },
        editor_task,
    )
    .await
}

async fn run_with_provider(
    agent: RecordingAgent,
    provider: NativeMcpProvider,
    editor_task: impl AsyncFnOnce(
        agent_client_protocol::ConnectionTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error>,
) -> Result<(), agent_client_protocol::Error> {
    drop(
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init(),
    );

    let (editor_out, conductor_in) = duplex(4096);
    let (conductor_out, editor_in) = duplex(4096);

    let transport =
        agent_client_protocol::ByteStreams::new(editor_out.compat_write(), editor_in.compat());

    Client
        .builder()
        .name("polyfill-test-client")
        .with_spawned(|_cx| async move {
            ConductorImpl::new_agent(
                "polyfill-test-conductor".to_string(),
                ProxiesAndAgent::new(agent)
                    .proxy(provider)
                    .proxy(McpOverAcpPolyfill::http()),
            )
            .run(agent_client_protocol::ByteStreams::new(
                conductor_out.compat_write(),
                conductor_in.compat(),
            ))
            .await
        })
        .connect_with(transport, editor_task)
        .await
}

#[tokio::test]
async fn http_downstream_receives_stable_transformed_declarations_for_all_setup_methods()
-> Result<(), agent_client_protocol::Error> {
    let observed = Arc::new(ObservedRequests::default());
    let agent = RecordingAgent {
        capabilities: agent_capabilities(McpCapabilities::new().http(true)),
        observed: observed.clone(),
    };
    let request_count = Arc::new(AtomicUsize::new(0));

    run_with_polyfill(agent, request_count.clone(), async |connection| {
        let initialize =
            recv(connection.send_request(InitializeRequest::new(ProtocolVersion::V1))).await?;
        assert!(initialize.agent_capabilities.mcp_capabilities.http);
        assert!(
            initialize.agent_capabilities.mcp_capabilities.acp,
            "the HTTP adapter should advertise native MCP support upstream"
        );

        let cwd = PathBuf::from("/tmp");
        let session =
            recv(connection.send_request(
                NewSessionRequest::new(cwd.clone()).mcp_servers(vec![native_server()]),
            ))
            .await?;
        recv(
            connection.send_request(
                LoadSessionRequest::new(session.session_id.clone(), cwd.clone())
                    .mcp_servers(vec![native_server()]),
            ),
        )
        .await?;
        recv(connection.send_request(
            ResumeSessionRequest::new(session.session_id, cwd).mcp_servers(vec![native_server()]),
        ))
        .await?;

        let (url, bearer) = {
            let setup = observed.setup.lock().unwrap();
            let McpServer::Http(server) = &setup[0].mcp_servers[0] else {
                panic!("expected HTTP declaration")
            };
            (server.url.clone(), server.headers[0].value.clone())
        };
        let (first, second) =
            tokio::join!(http_post(&url, &bearer, 1), http_post(&url, &bearer, 1),);
        assert_eq!(
            first,
            serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","tools":[]}})
        );
        assert_eq!(second, first);
        Ok(())
    })
    .await?;

    let setup = observed
        .setup
        .lock()
        .expect("setup request mutex should not be poisoned");
    assert_eq!(
        request_count.load(Ordering::SeqCst),
        2,
        "each HTTP POST creates exactly one native MCP request, without a connect handshake"
    );
    assert_eq!(setup.len(), 3);
    assert_eq!(setup[0].method, SetupMethod::New);
    assert_eq!(setup[1].method, SetupMethod::Load);
    assert_eq!(setup[2].method, SetupMethod::Resume);

    let expected_meta = serde_json::Map::from_iter([(
        "source".to_string(),
        serde_json::Value::String("integration-test".to_string()),
    )]);
    let mut endpoint = None;
    for request in setup.iter() {
        let [McpServer::Http(server)] = request.mcp_servers.as_slice() else {
            panic!(
                "expected one HTTP MCP declaration for {:?}, got {:?}",
                request.method, request.mcp_servers
            );
        };
        assert_eq!(server.name, SERVER_NAME);
        assert_eq!(server.meta.as_ref(), Some(&expected_meta));
        assert_eq!(server.headers.len(), 1);
        assert_eq!(server.headers[0].name, "Authorization");
        assert!(server.headers[0].value.starts_with("Bearer "));
        assert!(server.url.starts_with("http://127.0.0.1:"));
        if let Some(endpoint) = &endpoint {
            assert_eq!(
                &server.url, endpoint,
                "the same ACP server ID should reuse one listener"
            );
        } else {
            endpoint = Some(server.url.clone());
        }
    }

    Ok(())
}

#[tokio::test]
async fn native_downstream_keeps_capability_and_declaration_unchanged()
-> Result<(), agent_client_protocol::Error> {
    let observed = Arc::new(ObservedRequests::default());
    let agent = RecordingAgent {
        capabilities: agent_capabilities(McpCapabilities::new().acp(true)),
        observed: observed.clone(),
    };
    let declaration = native_server();
    let expected = declaration.clone();
    let request_count = Arc::new(AtomicUsize::new(0));

    run_with_polyfill(agent, request_count.clone(), async move |connection| {
        let initialize =
            recv(connection.send_request(InitializeRequest::new(ProtocolVersion::V1))).await?;
        assert!(!initialize.agent_capabilities.mcp_capabilities.http);
        assert!(initialize.agent_capabilities.mcp_capabilities.acp);

        recv(connection.send_request(
            NewSessionRequest::new(PathBuf::from("/tmp")).mcp_servers(vec![declaration]),
        ))
        .await?;
        Ok(())
    })
    .await?;

    let setup = observed
        .setup
        .lock()
        .expect("setup request mutex should not be poisoned");
    assert_eq!(setup.len(), 1);
    assert_eq!(setup[0].mcp_servers, vec![expected]);
    assert_eq!(
        request_count.load(Ordering::SeqCst),
        0,
        "a native-capable downstream should not be routed through the HTTP adapter"
    );

    Ok(())
}

#[tokio::test]
async fn unsupported_downstream_does_not_gain_native_capability()
-> Result<(), agent_client_protocol::Error> {
    let agent = RecordingAgent {
        capabilities: agent_capabilities(McpCapabilities::new()),
        observed: Arc::default(),
    };

    run_with_polyfill(agent, Arc::default(), async |connection| {
        let initialize =
            recv(connection.send_request(InitializeRequest::new(ProtocolVersion::V1))).await?;
        assert!(!initialize.agent_capabilities.mcp_capabilities.http);
        assert!(
            !initialize.agent_capabilities.mcp_capabilities.acp,
            "the adapter must not advertise native MCP without a usable downstream transport"
        );

        let error = recv(connection.send_request(
            NewSessionRequest::new(PathBuf::from("/tmp")).mcp_servers(vec![native_server()]),
        ))
        .await
        .expect_err("native declarations must not reach an unsupported downstream agent");
        assert_eq!(error.code, agent_client_protocol::ErrorCode::InvalidParams);
        assert_eq!(
            error.data,
            Some(serde_json::json!(
                "the downstream agent supports neither native nor HTTP MCP transport"
            ))
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn direct_v1_calls_preserve_inner_errors_without_hidden_listing()
-> Result<(), agent_client_protocol::Error> {
    let observed = Arc::new(ObservedRequests::default());
    let count = Arc::new(AtomicUsize::new(0));
    let ids = Arc::new(Mutex::new(Vec::new()));
    let agent = RecordingAgent {
        capabilities: agent_capabilities(McpCapabilities::new().http(true)),
        observed: observed.clone(),
    };
    run_with_provider(
        agent,
        NativeMcpProvider {
            request_count: count.clone(),
            request_ids: ids.clone(),
            ..Default::default()
        },
        async |connection| {
            recv(connection.send_request(InitializeRequest::new(ProtocolVersion::V1))).await?;
            recv(connection.send_request(
                NewSessionRequest::new(PathBuf::from("/tmp")).mcp_servers(vec![native_server()]),
            ))
            .await?;
            let (url, bearer) = {
                let observed = observed.setup.lock().unwrap();
                let McpServer::Http(server) = &observed[0].mcp_servers[0] else {
                    panic!("expected HTTP")
                };
                (server.url.clone(), server.headers[0].value.clone())
            };
            let call = post_json(
                &url,
                &bearer,
                serde_json::json!("external"),
                "tools/call",
                serde_json::json!({"name":"ping","arguments":{"text":"hello"}}),
            )
            .await;
            assert_eq!(
                call,
                serde_json::json!({"jsonrpc":"2.0","id":"external",
            "result":{"resultType":"complete","content":[],"_meta":{"fixture":"preserved"}}})
            );
            let peer_error = post_json(
                &url,
                &bearer,
                serde_json::json!(7),
                "tools/error",
                serde_json::json!({}),
            )
            .await;
            assert_eq!(
                peer_error["error"],
                serde_json::json!({"code":-33001,"message":"peer-owned",
            "data":null,"extension":{"preserve":true}})
            );
            let outer_error = post_json(
                &url,
                &bearer,
                serde_json::json!(7),
                "tools/backend-failure",
                serde_json::json!({}),
            )
            .await;
            assert_eq!(outer_error["id"], 7);
            assert_eq!(outer_error["error"]["code"], -33002);
            Ok(())
        },
    )
    .await?;
    assert_eq!(
        count.load(Ordering::SeqCst),
        3,
        "no hidden tools/list or handshake"
    );
    let ids = ids.lock().unwrap();
    assert_eq!(
        ids.iter().collect::<std::collections::HashSet<_>>().len(),
        3
    );
    Ok(())
}

#[tokio::test]
async fn v1_subscriptions_use_filter_and_ack_with_independent_stream_cancellation()
-> Result<(), agent_client_protocol::Error> {
    let observed = Arc::new(ObservedRequests::default());
    let ids = Arc::new(Mutex::new(Vec::new()));
    let cancelled = Arc::new(Mutex::new(Vec::new()));
    let agent = RecordingAgent {
        capabilities: agent_capabilities(McpCapabilities::new().http(true)),
        observed: observed.clone(),
    };
    run_with_provider(
        agent,
        NativeMcpProvider {
            request_ids: ids.clone(),
            cancelled_ids: cancelled.clone(),
            ..Default::default()
        },
        async |connection| {
            recv(connection.send_request(InitializeRequest::new(ProtocolVersion::V1))).await?;
            recv(connection.send_request(
                NewSessionRequest::new(PathBuf::from("/tmp")).mcp_servers(vec![native_server()]),
            ))
            .await?;
            let (url, bearer) = {
                let observed = observed.setup.lock().unwrap();
                let McpServer::Http(server) = &observed[0].mcp_servers[0] else {
                    panic!("expected HTTP")
                };
                (server.url.clone(), server.headers[0].value.clone())
            };
            let params = serde_json::json!({"notifications":{"toolsListChanged":true}});
            let mut first = open_post(
                &url,
                &bearer,
                serde_json::json!(73),
                "subscriptions/listen",
                params.clone(),
            )
            .await;
            let first_events = subscription_events(&mut first).await;
            let mut second = open_post(
                &url,
                &bearer,
                serde_json::json!(73),
                "subscriptions/listen",
                params,
            )
            .await;
            let second_events = subscription_events(&mut second).await;
            for events in [first_events, second_events] {
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
            let logical_ids = ids.lock().unwrap().clone();
            assert_eq!(logical_ids.len(), 2);
            assert_ne!(logical_ids[0], logical_ids[1]);
            drop(first);
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while cancelled.lock().unwrap().len() != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("first stream closure must cancel its native request");
            assert_eq!(*cancelled.lock().unwrap(), vec![logical_ids[0].clone()]);
            // Ordinary calls still work while the sibling subscription remains open.
            assert_eq!(
                http_post(&url, &bearer, 73).await["result"]["tools"],
                serde_json::json!([])
            );
            assert_eq!(cancelled.lock().unwrap().len(), 1);
            drop(second);
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while cancelled.lock().unwrap().len() != 2 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("second stream closure must cancel its own native request");
            assert_eq!(*cancelled.lock().unwrap(), logical_ids);
            Ok(())
        },
    )
    .await
}
