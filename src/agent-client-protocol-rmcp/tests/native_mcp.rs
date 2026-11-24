//! Real rmcp service lifecycle through native ACP carriers and raw Channels.
#![cfg(all(feature = "rmcp_3", feature = "unstable_mcp_over_acp"))]

use agent_client_protocol::{
    Agent, Channel, Client, ConnectionTo, Error, RawJsonRpcMessage, RunWithConnectionTo,
    TransportFrame, UntypedMessage,
    mcp_server::{McpConnectionTo, McpServer},
    role::mcp,
    schema::ProtocolVersion,
    v2::schema as v2,
};
use agent_client_protocol_rmcp::rmcp_3::{McpRequestAttribution, McpServerExt};
use futures::StreamExt;
extern crate rmcp_3 as rmcp;

use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::tool::{Extension, ToolCallContext, ToolRouter},
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, DiscoverResult,
        InputRequiredResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
        ServerConfig, SubscriptionFilter,
    },
    service::{RequestContext, SubscriptionContext},
    tool, tool_router,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot;

const TIMEOUT: Duration = Duration::from_secs(10);

struct DropSignal(Option<oneshot::Sender<()>>);
impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _sent = tx.send(());
        }
    }
}
type Probe = Arc<Mutex<Option<(oneshot::Sender<()>, oneshot::Sender<()>)>>>;

struct Service {
    calls: Arc<AtomicUsize>,
    hang: Probe,
    subscription: Probe,
}
impl ServerHandler for Service {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        cx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if request.name.as_ref() == "hang" {
            let (started, dropped) = self.hang.lock().unwrap().take().unwrap();
            let _drop = DropSignal(Some(dropped));
            let _sent = started.send(());
            // Ignore cx.ct deliberately. The adapter must own and drop this
            // actual handler future, not just cancel the rmcp actor.
            std::future::pending::<()>().await;
        }
        match request.name.as_ref() {
            "retry" if request.request_state.is_none() => {
                let inputs = serde_json::from_value(json!({"confirmation":{
                    "method":"elicitation/create","params":{"mode":"form","message":"Confirm",
                    "requestedSchema":{"type":"object","properties":{"approved":{"type":"boolean"}}}}
                }})).unwrap();
                Ok(
                    InputRequiredResult::new(Some(inputs), Some("opaque-retry-state".into()))
                        .into(),
                )
            }
            "retry" => Ok(CallToolResult::structured(json!({
                "marker":cx.meta.get("example/marker"),"responses":request.input_responses
            }))
            .into()),
            "echo" => Ok(CallToolResult::structured(
                json!({"marker":cx.meta.get("example/marker")}),
            )
            .into()),
            _ => Err(ErrorData::invalid_params(
                "unknown tool",
                Some(json!({"origin":"rmcp"})),
            )),
        }
    }
    fn accepted_subscription_filter(
        &self,
        filter: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        Some(filter.clone())
    }
    async fn listen(&self, cx: SubscriptionContext) -> Result<(), ErrorData> {
        let (started, dropped) = self.subscription.lock().unwrap().take().unwrap();
        let _drop = DropSignal(Some(dropped));
        cx.sink()
            .notify_tool_list_changed()
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let _sent = started.send(());
        cx.cancelled().await;
        Ok(())
    }
}

fn send(peer: &Channel, value: Value) {
    let message: RawJsonRpcMessage = serde_json::from_value(value).unwrap();
    peer.tx
        .unbounded_send(TransportFrame::Single(message))
        .unwrap();
}
async fn receive(peer: &mut Channel) -> Value {
    let TransportFrame::Single(message) = peer.rx.next().await.expect("frame") else {
        panic!("single frame")
    };
    serde_json::to_value(message).unwrap()
}
fn params(mut params: Value, marker: &str) -> Value {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{"elicitation":{"form":{}}},
        "example/marker":marker
    });
    params
}
fn request(
    peer: &Channel,
    server: &Value,
    outer: &str,
    logical: &str,
    method: &str,
    params: Value,
) {
    send(
        peer,
        json!({"jsonrpc":"2.0","id":outer,"method":"mcp/message","params":{
            "serverId":server,"requestId":logical,"method":method,"params":params
        }}),
    );
}
async fn response(peer: &mut Channel, id: &str) -> Value {
    loop {
        let value = receive(peer).await;
        if value.get("id") == Some(&json!(id)) {
            return value;
        }
        assert_eq!(value["method"], "mcp/message", "{value}");
    }
}

async fn host<Run: RunWithConnectionTo<Agent> + 'static>(
    server: McpServer<Agent, Run>,
) -> (
    Channel,
    Value,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<Result<(), Error>>,
) {
    let (transport, mut peer) = Channel::duplex();
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(Client.v1().connect_with(transport, async move |cx| {
        cx.build_session_cwd()?
            .with_mcp_server(server)?
            .block_task()
            .run_until(async |_session| {
                let eof = cx.incoming_closed();
                futures::pin_mut!(eof);
                let _stopped = futures::future::select(stopped, eof).await;
                Ok(())
            })
            .await?;
        Ok(())
    }));
    let setup = receive(&mut peer).await;
    let server = setup["params"]["mcpServers"][0]["serverId"].clone();
    assert!(server.is_string());
    send(
        &peer,
        json!({"jsonrpc":"2.0","id":setup["id"],"result":{"sessionId":"native"}}),
    );
    (peer, server, stop, task)
}

fn fixture() -> (
    McpServer<Agent>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Probe,
    Probe,
) {
    let factories = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let hang = Arc::new(Mutex::new(None));
    let subscription = Arc::new(Mutex::new(None));
    let server = McpServer::from_rmcp("native", {
        let factories = factories.clone();
        let calls = calls.clone();
        let hang = hang.clone();
        let subscription = subscription.clone();
        move || {
            factories.fetch_add(1, Ordering::SeqCst);
            Service {
                calls: calls.clone(),
                hang: hang.clone(),
                subscription: subscription.clone(),
            }
        }
    });
    assert_eq!(factories.load(Ordering::SeqCst), 0, "factory must be lazy");
    (server, factories, calls, hang, subscription)
}
fn probe(slot: &Probe) -> (oneshot::Receiver<()>, oneshot::Receiver<()>) {
    let (started, ready) = oneshot::channel();
    let (dropped, done) = oneshot::channel();
    *slot.lock().unwrap() = Some((started, dropped));
    (ready, done)
}

#[tokio::test]
async fn shared_rmcp_is_lazy_once_with_metadata_mrtr_errors_and_subscriptions() {
    tokio::time::timeout(TIMEOUT, async {
        let (server, factories, calls, hang, subscription) = fixture();
        let (mut peer, server, stop, task) = host(server).await;
        assert_eq!(factories.load(Ordering::SeqCst), 0);
        request(&peer, &server, "echo", "first", "tools/call", params(json!({"name":"echo","arguments":{}}), "first"));
        assert_eq!(response(&mut peer, "echo").await["result"]["result"]["structuredContent"]["marker"], "first");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no hidden initialization/discovery");
        request(&peer, &server, "retry", "retry", "tools/call", params(json!({"name":"retry","arguments":{}}), "retry"));
        let first = response(&mut peer, "retry").await["result"]["result"].clone();
        assert_eq!(first["resultType"], "input_required");
        let answers = json!({"confirmation":{"action":"accept","content":{"approved":true}}});
        request(&peer, &server, "retry2", "retry2", "tools/call",
            params(json!({"name":"retry","arguments":{},"requestState":first["requestState"],"inputResponses":answers}), "second"));
        let second = response(&mut peer, "retry2").await;
        assert_eq!(second["result"]["result"]["structuredContent"]["marker"], "second");
        assert_eq!(second["result"]["result"]["structuredContent"]["responses"], answers);
        request(&peer, &server, "error", "error", "tools/call", params(json!({"name":"missing","arguments":{}}), "error"));
        assert_eq!(response(&mut peer, "error").await["result"]["error"], json!({"code":-32602,"message":"unknown tool","data":{"origin":"rmcp"}}));

        let (started, dropped) = probe(&subscription);
        request(&peer, &server, "listen", "listen", "subscriptions/listen",
            params(json!({"notifications":{"toolsListChanged":true}}), "listen"));
        started.await.unwrap();
        let mut changed = false;
        while !changed {
            let notification = receive(&mut peer).await;
            assert_eq!(notification["params"]["serverId"], server);
            assert_eq!(notification["params"]["requestId"], "listen");
            changed = notification["params"]["method"] == "notifications/tools/list_changed";
        }
        request(&peer, &server, "parallel", "parallel", "tools/call", params(json!({"name":"echo","arguments":{}}), "parallel"));
        assert_eq!(response(&mut peer, "parallel").await["result"]["result"]["structuredContent"]["marker"], "parallel");
        send(&peer, json!({"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":"listen"}}));
        let cancelled = response(&mut peer, "listen").await;
        assert!(cancelled["error"].is_object(), "{cancelled}");
        dropped.await.unwrap();

        let (started, mut dropped) = probe(&hang);
        request(&peer, &server, "hang", "duplicate", "tools/call", params(json!({"name":"hang","arguments":{}}), "hang"));
        started.await.unwrap();
        request(&peer, &server, "duplicate", "duplicate", "tools/call", params(json!({"name":"echo","arguments":{}}), "duplicate"));
        assert_eq!(response(&mut peer, "duplicate").await["error"]["code"], -32602);
        send(&peer, json!({"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":"hang"}}));
        assert!(response(&mut peer, "hang").await["error"].is_object());
        assert!(matches!(dropped.try_recv(), Ok(())), "response preceded actual handler destruction");
        request(&peer, &server, "reuse", "duplicate", "tools/call", params(json!({"name":"echo","arguments":{}}), "reuse"));
        assert_eq!(response(&mut peer, "reuse").await["result"]["result"]["structuredContent"]["marker"], "reuse");
        assert_eq!(factories.load(Ordering::SeqCst), 1, "one shared native application service");
        let _sent = stop.send(());
        task.await.unwrap().unwrap();
    }).await.expect("native rmcp lifecycle timed out");
}

#[tokio::test]
async fn real_rmcp_handler_is_destroyed_before_connection_returns_on_eof() {
    tokio::time::timeout(TIMEOUT, async {
        let (server, _, _, hang, _) = fixture();
        let (peer, server, _stop, task) = host(server).await;
        let (started, mut dropped) = probe(&hang);
        request(
            &peer,
            &server,
            "hang",
            "eof",
            "tools/call",
            params(json!({"name":"hang","arguments":{}}), "eof"),
        );
        started.await.unwrap();
        drop(peer);
        task.await.unwrap().unwrap();
        assert!(
            matches!(dropped.try_recv(), Ok(())),
            "EOF discarded handler cleanup"
        );
    })
    .await
    .expect("rmcp EOF cleanup timed out");
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct ToolArgs {
    hang: bool,
}

async fn scoped_closure_cleanup(mutable: bool, eof: bool) {
    let slot: Probe = Arc::new(Mutex::new(None));
    let (started, mut dropped) = probe(&slot);
    let operation = {
        let slot = slot.clone();
        async move |args: ToolArgs, _cx: McpConnectionTo<Agent>| {
            if args.hang {
                let (started, dropped) = slot.lock().unwrap().take().unwrap();
                let _drop = DropSignal(Some(dropped));
                let _sent = started.send(());
                std::future::pending::<()>().await;
            }
            Ok::<_, Error>("completed".to_owned())
        }
    };
    let (mut peer, server, stop, task) = if mutable {
        host(
            McpServer::builder("closure")
                .tool_fn_mut(
                    "closure",
                    "cleanup probe",
                    operation,
                    agent_client_protocol_rmcp::rmcp_3::tool_fn_mut!(),
                )
                .build(),
        )
        .await
    } else {
        host(
            McpServer::builder("closure")
                .tool_fn(
                    "closure",
                    "cleanup probe",
                    operation,
                    agent_client_protocol_rmcp::rmcp_3::tool_fn!(),
                )
                .build(),
        )
        .await
    };
    request(
        &peer,
        &server,
        "hang",
        "closure",
        "tools/call",
        params(json!({"name":"closure","arguments":{"hang":true}}), "hang"),
    );
    started.await.unwrap();
    if eof {
        drop(peer);
        task.await.unwrap().unwrap();
    } else {
        send(
            &peer,
            json!({"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":"hang"}}),
        );
        assert!(response(&mut peer, "hang").await["error"].is_object());
        assert!(
            matches!(dropped.try_recv(), Ok(())),
            "response preceded actual closure destruction"
        );
        request(
            &peer,
            &server,
            "next",
            "closure",
            "tools/call",
            params(json!({"name":"closure","arguments":{"hang":false}}), "next"),
        );
        assert!(response(&mut peer, "next").await["result"]["result"]["content"].is_array());
        let _sent = stop.send(());
        task.await.unwrap().unwrap();
        return;
    }
    assert!(
        matches!(dropped.try_recv(), Ok(())),
        "EOF discarded closure cleanup"
    );
}

#[tokio::test]
async fn function_tool_runners_join_actual_closure_cleanup_on_cancellation_and_eof() {
    tokio::time::timeout(TIMEOUT, async {
        for mutable in [false, true] {
            for eof in [false, true] {
                scoped_closure_cleanup(mutable, eof).await;
            }
        }
    })
    .await
    .expect("scoped closure cleanup timed out");
}

type Host = (
    Channel,
    Value,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<Result<(), Error>>,
);

/// Like [`host`], for a v2 client.
async fn host_v2<Run: RunWithConnectionTo<Agent> + 'static>(server: McpServer<Agent, Run>) -> Host {
    let (transport, mut peer) = Channel::duplex();
    let (stop, stopped) = oneshot::channel();
    let implementation = || v2::Implementation::new("native-mcp-test", "1");
    let task = tokio::spawn(Client.v2().connect_with(transport, async move |cx| {
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
        let eof = cx.incoming_closed();
        futures::pin_mut!(eof);
        let _stopped = futures::future::select(stopped, eof).await;
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
            "jsonrpc":"2.0",
            "id":initialize["id"],
            "result":v2::InitializeResponse::new(ProtocolVersion::V2, implementation())
                .capabilities(capabilities),
        }),
    );
    let setup = receive(&mut peer).await;
    let server = setup["params"]["mcpServers"][0]["serverId"].clone();
    assert!(server.is_string());
    send(
        &peer,
        json!({"jsonrpc":"2.0","id":setup["id"],"result":{"sessionId":"native"}}),
    );
    (peer, server, stop, task)
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct NoArgs {}

/// Tools that need the session of the request that calls them.
fn session_tools() -> McpServer<Agent, impl RunWithConnectionTo<Agent> + 'static> {
    McpServer::builder("sessions")
        .tool_fn(
            "require",
            "Returns the session of the request",
            async |_args: NoArgs, cx: McpConnectionTo<Agent>| {
                Ok::<_, Error>(cx.require_session_id()?.to_string())
            },
            agent_client_protocol_rmcp::rmcp_3::tool_fn!(),
        )
        .tool_fn_mut(
            "require_mut",
            "Returns the session of the request",
            async |_args: NoArgs, cx: McpConnectionTo<Agent>| {
                Ok::<_, Error>(cx.require_session_id()?.to_string())
            },
            agent_client_protocol_rmcp::rmcp_3::tool_fn_mut!(),
        )
        .tool_fn(
            "reject",
            "Refuses the session of the request",
            async |_args: NoArgs, cx: McpConnectionTo<Agent>| -> Result<String, Error> {
                Err(cx.reject_session("the session is not active"))
            },
            agent_client_protocol_rmcp::rmcp_3::tool_fn!(),
        )
        .tool_fn(
            "fail",
            "Fails without rejecting the request",
            async |_args: NoArgs, _cx: McpConnectionTo<Agent>| -> Result<String, Error> {
                Err(Error::invalid_params().data("not recorded as a rejection"))
            },
            agent_client_protocol_rmcp::rmcp_3::tool_fn!(),
        )
        .build()
}

/// Sends `method` in an `mcp/message` request that carries `attribution`, and
/// returns the response.
async fn attributed(
    peer: &mut Channel,
    server: &Value,
    id: &str,
    method: &str,
    inner: Value,
    attribution: Value,
) -> Value {
    let mut envelope = json!({
        "serverId":server,"requestId":id,"method":method,"params":params(inner, id)
    });
    envelope
        .as_object_mut()
        .unwrap()
        .extend(attribution.as_object().unwrap().clone());
    send(
        peer,
        json!({"jsonrpc":"2.0","id":id,"method":"mcp/message","params":envelope}),
    );
    response(peer, id).await
}

/// Calls `tool` in an `mcp/message` request that carries `attribution`, and
/// returns the response.
async fn call_tool(
    peer: &mut Channel,
    server: &Value,
    id: &str,
    tool: &str,
    attribution: Value,
) -> Value {
    let inner = json!({"name":tool,"arguments":{}});
    attributed(peer, server, id, "tools/call", inner, attribution).await
}

async fn tools_reject_requests_for_their_session((mut peer, server, stop, task): Host) {
    let child = json!({"sessionId":"child"});
    for tool in ["require", "require_mut"] {
        // Without a session the request is rejected. The tool's own `Err`
        // would only be an `isError` tool result.
        let missing = call_tool(&mut peer, &server, "missing", tool, json!({})).await;
        assert_eq!(
            missing["error"],
            json!({
                "code":-32602,"message":"Invalid params",
                "data":"this MCP server requires sessionId on mcp/message"
            }),
            "{tool}"
        );
        assert!(missing.get("result").is_none(), "{tool}: {missing}");
        // The same tool, and the runner of a `tool_fn_mut`, still serve the
        // next request.
        let ran = call_tool(&mut peer, &server, "present", tool, child.clone()).await;
        assert_eq!(
            ran["result"]["result"]["content"][0]["text"], "\"child\"",
            "{tool}: {ran}"
        );
        assert_eq!(ran["result"]["result"]["isError"], false, "{tool}: {ran}");
    }

    let refused = call_tool(&mut peer, &server, "refused", "reject", child.clone()).await;
    assert_eq!(
        refused["error"],
        json!({
            "code":-32602,"message":"Invalid params",
            "data":"sessionId rejected: the session is not active"
        })
    );

    // An error that is not recorded as a rejection stays a tool result.
    let failed = call_tool(&mut peer, &server, "failed", "fail", child).await;
    assert!(failed.get("error").is_none(), "{failed}");
    assert_eq!(failed["result"]["result"]["isError"], true, "{failed}");
    assert!(
        failed["result"]["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("not recorded as a rejection")),
        "{failed}"
    );

    let _sent = stop.send(());
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn function_tools_reject_requests_for_their_session() {
    tokio::time::timeout(TIMEOUT, async {
        tools_reject_requests_for_their_session(host(session_tools()).await).await;
        tools_reject_requests_for_their_session(host_v2(session_tools()).await).await;
    })
    .await
    .expect("session rejection timed out");
}

/// An rmcp handler that reads the session and tool call of a request, and
/// needs a session for discovery and to list or call its tools.
struct SessionHandler;

impl SessionHandler {
    fn discovery(&self, context: &RequestContext<RoleServer>) -> Result<DiscoverResult, ErrorData> {
        McpRequestAttribution::of(context).require_session_id()?;
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            self.get_info(),
        ))
    }

    fn tools(context: &RequestContext<RoleServer>) -> Result<ListToolsResult, ErrorData> {
        McpRequestAttribution::of(context).require_session_id()?;
        Ok(ListToolsResult::with_all_items(Vec::new())
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Private))
    }

    fn tool(
        request: &CallToolRequestParams,
        context: &RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let attribution = McpRequestAttribution::of(context);
        match request.name.as_ref() {
            "attribution" => Ok(CallToolResult::structured(json!({
                "sessionId":attribution.session_id(),
                "toolCallId":attribution.tool_call_id(),
            }))
            .into()),
            "require" => {
                let session_id = attribution.require_session_id()?;
                Ok(CallToolResult::structured(json!({"sessionId":session_id})).into())
            }
            "reject" => Err(attribution.reject_session("the session is not active")),
            // A handler that needs the host connection names its role.
            "connection" => {
                let agent = context.extensions.get::<McpConnectionTo<Agent>>();
                let mcp_client = context.extensions.get::<McpConnectionTo<mcp::Client>>();
                Ok(CallToolResult::structured(json!({
                    "agent":agent.is_some(),
                    "mcpClient":mcp_client.is_some(),
                    "sessionId":agent.and_then(McpConnectionTo::session_id),
                }))
                .into())
            }
            _ => Err(ErrorData::invalid_params("unknown tool", None)),
        }
    }
}

impl ServerHandler for SessionHandler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
    fn discover(
        &self,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<DiscoverResult, ErrorData>> + Send {
        std::future::ready(self.discovery(&context))
    }
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send {
        std::future::ready(Self::tools(&context))
    }
    fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + Send {
        std::future::ready(Self::tool(&request, &context))
    }
}

/// An rmcp handler whose `#[tool]` methods take the attribution as a
/// parameter.
struct ToolHandler {
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl ToolHandler {
    #[tool(description = "Returns the session and tool call of the request")]
    fn read(attribution: McpRequestAttribution) -> CallToolResult {
        CallToolResult::structured(json!({
            "sessionId":attribution.session_id(),
            "toolCallId":attribution.tool_call_id(),
        }))
    }

    #[tool(description = "Returns the session of the request")]
    async fn require(
        &self,
        attribution: McpRequestAttribution,
    ) -> Result<CallToolResult, ErrorData> {
        let session_id = attribution.require_session_id()?;
        Ok(CallToolResult::structured(json!({"sessionId":session_id})))
    }

    #[tool(description = "Refuses the session of the request")]
    fn reject(attribution: McpRequestAttribution) -> ErrorData {
        attribution.reject_session("the session is not active")
    }

    /// rmcp's own extractor finds the attribution only where it is attached.
    #[tool(description = "Returns the session of the request")]
    fn from_extension(
        Extension(attribution): Extension<McpRequestAttribution>,
    ) -> Result<CallToolResult, ErrorData> {
        let session_id = attribution.require_session_id()?;
        Ok(CallToolResult::structured(json!({"sessionId":session_id})))
    }
}

impl ServerHandler for ToolHandler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let context = ToolCallContext::new(self, request, context);
        self.tool_router.call(context).await
    }
}

fn tool_server<R: agent_client_protocol::Role>() -> McpServer<R> {
    McpServer::from_rmcp("tools", || ToolHandler {
        tool_router: ToolHandler::tool_router(),
    })
}

fn missing_session() -> Value {
    json!({
        "code":-32602,"message":"Invalid params",
        "data":"this MCP server requires sessionId on mcp/message"
    })
}

async fn handler_reads_attribution_and_rejects((mut peer, server, stop, task): Host) {
    let structured = |response: &Value| response["result"]["result"]["structuredContent"].clone();
    let child = json!({"sessionId":"child"});

    let attribution = json!({"sessionId":"child","toolCallId":"call_1"});
    let seen = call_tool(
        &mut peer,
        &server,
        "seen",
        "attribution",
        attribution.clone(),
    )
    .await;
    assert_eq!(structured(&seen), attribution);
    let unattributed = call_tool(&mut peer, &server, "none", "attribution", json!({})).await;
    assert_eq!(
        structured(&unattributed),
        json!({"sessionId":null,"toolCallId":null})
    );

    // Without a session, `call_tool`, `list_tools` and `discover` reject the
    // request. The handler's own `Err` would only be an inner MCP error.
    let require = json!({"name":"require","arguments":{}});
    for (method, inner, expected) in [
        ("tools/call", require, json!({"structuredContent":child})),
        ("tools/list", json!({}), json!({"tools":[]})),
        (
            "server/discover",
            json!({}),
            json!({"supportedVersions":["2026-07-28"]}),
        ),
    ] {
        let missing = attributed(&mut peer, &server, "none", method, inner.clone(), json!({}));
        let missing = missing.await;
        assert_eq!(missing["error"], missing_session(), "{method}");
        assert!(missing.get("result").is_none(), "{method}: {missing}");

        let present = attributed(&mut peer, &server, "some", method, inner, child.clone());
        let present = present.await;
        assert!(present.get("error").is_none(), "{method}: {present}");
        for (key, value) in expected.as_object().unwrap() {
            assert_eq!(
                &present["result"]["result"][key], value,
                "{method}: {present}"
            );
        }
    }

    let refused = call_tool(&mut peer, &server, "refused", "reject", child.clone()).await;
    assert_eq!(
        refused["error"],
        json!({
            "code":-32602,"message":"Invalid params",
            "data":"sessionId rejected: the session is not active"
        })
    );

    // The handler also finds the host connection, under the role the server
    // was attached to and no other.
    let connection = call_tool(&mut peer, &server, "connection", "connection", child).await;
    assert_eq!(
        structured(&connection),
        json!({"agent":true,"mcpClient":false,"sessionId":"child"})
    );

    let _sent = stop.send(());
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn rmcp_handler_reads_attribution_and_rejects_requests_for_their_session() {
    tokio::time::timeout(TIMEOUT, async {
        let server = || McpServer::<Agent>::from_rmcp("sessions", || SessionHandler);
        handler_reads_attribution_and_rejects(host(server()).await).await;
        handler_reads_attribution_and_rejects(host_v2(server()).await).await;
    })
    .await
    .expect("rmcp handler session test timed out");
}

async fn tool_methods_take_the_attribution((mut peer, server, stop, task): Host) {
    let structured = |response: &Value| response["result"]["result"]["structuredContent"].clone();
    let child = json!({"sessionId":"child"});

    let attribution = json!({"sessionId":"child","toolCallId":"call_1"});
    let seen = call_tool(&mut peer, &server, "seen", "read", attribution.clone()).await;
    assert_eq!(structured(&seen), attribution);
    let unattributed = call_tool(&mut peer, &server, "none", "read", json!({})).await;
    assert_eq!(
        structured(&unattributed),
        json!({"sessionId":null,"toolCallId":null})
    );

    for tool in ["require", "from_extension"] {
        let missing = call_tool(&mut peer, &server, "none", tool, json!({})).await;
        assert_eq!(missing["error"], missing_session(), "{tool}");
        assert!(missing.get("result").is_none(), "{tool}: {missing}");

        let present = call_tool(&mut peer, &server, "some", tool, child.clone()).await;
        assert_eq!(structured(&present), child, "{tool}: {present}");
    }

    let refused = call_tool(&mut peer, &server, "refused", "reject", child).await;
    assert_eq!(
        refused["error"],
        json!({
            "code":-32602,"message":"Invalid params",
            "data":"sessionId rejected: the session is not active"
        })
    );

    let _sent = stop.send(());
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn rmcp_tool_methods_take_the_attribution_as_a_parameter() {
    tokio::time::timeout(TIMEOUT, async {
        tool_methods_take_the_attribution(host(tool_server::<Agent>()).await).await;
        tool_methods_take_the_attribution(host_v2(tool_server::<Agent>()).await).await;
    })
    .await
    .expect("rmcp tool method test timed out");
}

/// Sends `method` to a standalone `server` and returns the result or the
/// error the MCP client receives.
async fn standalone_request(
    connection: &ConnectionTo<mcp::Server>,
    method: &str,
    inner: Value,
) -> Result<Value, Value> {
    let request = UntypedMessage::new(method, params(inner, method)).unwrap();
    connection
        .send_request(request)
        .block_task()
        .await
        .map_err(|error| serde_json::to_value(error).unwrap())
}

/// A standalone connection has no ACP session and no `mcp/message` request,
/// so the same handler reads no attribution and its errors are ordinary MCP
/// errors.
#[tokio::test]
async fn standalone_rmcp_handler_reads_no_attribution() -> Result<(), Error> {
    tokio::time::timeout(TIMEOUT, async {
        let server = McpServer::<mcp::Client>::from_rmcp("sessions", || SessionHandler);
        mcp::Client
            .builder()
            .connect_with(server, async |connection| {
                let call = async |tool: &str| {
                    let inner = json!({"name":tool,"arguments":{}});
                    standalone_request(&connection, "tools/call", inner).await
                };
                assert_eq!(
                    call("attribution").await.unwrap()["structuredContent"],
                    json!({"sessionId":null,"toolCallId":null})
                );
                assert_eq!(
                    call("connection").await.unwrap()["structuredContent"],
                    json!({"agent":false,"mcpClient":false,"sessionId":null})
                );

                let no_session = json!({
                    "code":-32602,"message":"a standalone MCP connection has no ACP session"
                });
                assert_eq!(call("require").await.unwrap_err(), no_session);
                for method in ["tools/list", "server/discover"] {
                    let error = standalone_request(&connection, method, json!({})).await;
                    assert_eq!(error.unwrap_err(), no_session, "{method}");
                }
                assert_eq!(
                    call("reject").await.unwrap_err(),
                    json!({
                        "code":-32602,
                        "message":"sessionId rejected: the session is not active"
                    })
                );
                Ok(())
            })
            .await
    })
    .await
    .expect("standalone rmcp handler test timed out")
}

/// On a standalone connection a `#[tool]` parameter is an attribution without
/// a session. rmcp's `Extension` extractor fails there, because nothing is
/// attached.
#[tokio::test]
async fn standalone_tool_methods_take_an_attribution_without_a_session() -> Result<(), Error> {
    tokio::time::timeout(TIMEOUT, async {
        mcp::Client
            .builder()
            .connect_with(tool_server::<mcp::Client>(), async |connection| {
                let call = async |tool: &str| {
                    let inner = json!({"name":tool,"arguments":{}});
                    standalone_request(&connection, "tools/call", inner).await
                };
                assert_eq!(
                    call("read").await.unwrap()["structuredContent"],
                    json!({"sessionId":null,"toolCallId":null})
                );
                assert_eq!(
                    call("require").await.unwrap_err(),
                    json!({
                        "code":-32602,
                        "message":"a standalone MCP connection has no ACP session"
                    })
                );
                assert_eq!(
                    call("reject").await.unwrap_err(),
                    json!({
                        "code":-32602,
                        "message":"sessionId rejected: the session is not active"
                    })
                );
                let error = call("from_extension").await.unwrap_err();
                assert_eq!(error["code"], -32602, "{error}");
                assert!(
                    error["message"]
                        .as_str()
                        .is_some_and(|message| message.starts_with("missing extension")),
                    "{error}"
                );
                Ok(())
            })
            .await
    })
    .await
    .expect("standalone tool method test timed out")
}
