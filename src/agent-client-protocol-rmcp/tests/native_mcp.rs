//! Real rmcp service lifecycle through native ACP carriers and raw Channels.
#![cfg(all(feature = "rmcp_3", feature = "unstable_mcp_over_acp"))]

use agent_client_protocol::{
    Agent, Channel, Client, ConnectionTo, Error, RawJsonRpcMessage, Role, RunWithConnectionTo,
    TransportFrame, UntypedMessage,
    mcp_server::{McpAdmission, McpConnectionTo, McpRejection, McpServer, McpTool},
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

/// Sends `method` in an `mcp/message` request that carries `attribution`, and
/// returns the response.
async fn attributed(
    peer: &mut Channel,
    server: &Value,
    method: &str,
    inner: Value,
    attribution: Value,
) -> Value {
    let mut envelope = json!({
        "serverId":server,"requestId":"request","method":method,
        "params":params(inner, "request")
    });
    envelope
        .as_object_mut()
        .unwrap()
        .extend(attribution.as_object().unwrap().clone());
    send(
        peer,
        json!({"jsonrpc":"2.0","id":"request","method":"mcp/message","params":envelope}),
    );
    response(peer, "request").await
}

/// Calls `tool` in an `mcp/message` request that carries `attribution`, and
/// returns the response.
async fn call_tool(peer: &mut Channel, server: &Value, tool: &str, attribution: Value) -> Value {
    let inner = json!({"name":tool,"arguments":{}});
    attributed(peer, server, "tools/call", inner, attribution).await
}

/// Sends `method` to a standalone server and returns the result, or the
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

/// Whether `response` is the outer error Invalid params with `reason`.
#[track_caller]
fn assert_rejected(response: &Value, reason: &str) {
    assert_eq!(
        response.get("error"),
        Some(&json!({"code":-32602,"message":"Invalid params","data":reason})),
        "{response}"
    );
    assert_eq!(response.get("result"), None, "{response}");
}

const SESSION_REQUIRED: &str = "this MCP server requires sessionId on mcp/message";

/// The check of a tool that needs the session of a call.
fn require_session(request: &McpAdmission<'_>) -> Result<(), McpRejection> {
    request.require_session_id()?;
    Ok(())
}

/// The text of the tool result in `response`.
fn tool_text(response: &Value) -> &Value {
    &response["result"]["result"]["content"][0]["text"]
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct NoArgs {}

/// A tool that states its admission rule next to its definition. It counts
/// its runs and returns the session it ran for.
struct Notes {
    runs: Arc<AtomicUsize>,
}

impl<R: Role> McpTool<R> for Notes {
    type Input = NoArgs;
    type Output = String;

    fn name(&self) -> String {
        "notes".into()
    }

    fn description(&self) -> String {
        "Returns the session whose notes it lists".into()
    }

    fn admit(&self, request: &McpAdmission<'_>) -> Result<(), McpRejection> {
        require_session(request)
    }

    fn call_tool(
        &self,
        _input: NoArgs,
        cx: McpConnectionTo<R>,
    ) -> impl Future<Output = Result<String, Error>> + Send {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let session = cx.session_id().map(ToString::to_string);
        std::future::ready(Ok(session.unwrap_or_default()))
    }
}

/// A builder server with one tool for each way to state a tool's rule, and a
/// tool without one. Every tool counts its runs in `runs`.
fn gated_tools<R: Role>(
    runs: Arc<AtomicUsize>,
) -> McpServer<R, impl RunWithConnectionTo<R> + 'static> {
    let counted = |runs: &Arc<AtomicUsize>| {
        let runs = runs.clone();
        async move |_args: NoArgs, _cx: McpConnectionTo<R>| {
            runs.fetch_add(1, Ordering::SeqCst);
            Ok::<_, Error>("ran".to_owned())
        }
    };
    McpServer::builder("gated")
        .tool(Notes { runs: runs.clone() })
        .tool_fn(
            "add_note",
            "Has a rule added by name",
            counted(&runs),
            agent_client_protocol_rmcp::rmcp_3::tool_fn!(),
        )
        .tool_fn_mut(
            "count_notes",
            "Has a rule added by name",
            counted(&runs),
            agent_client_protocol_rmcp::rmcp_3::tool_fn_mut!(),
        )
        .tool_fn(
            "help",
            "Has no rule",
            counted(&runs),
            agent_client_protocol_rmcp::rmcp_3::tool_fn!(),
        )
        .tool_admission("add_note", require_session)
        .unwrap()
        .tool_admission("count_notes", require_session)
        .unwrap()
        // A second check for one tool runs after the first.
        .tool_admission("add_note", |request| match request.session_id() {
            Some(session_id) if session_id.0.as_ref() == "other" => {
                Err(McpRejection::session_rejected("not a session with notes"))
            }
            _ => Ok(()),
        })
        .unwrap()
        .build()
}

async fn tool_rules_decide_before_the_tool_runs(
    (mut peer, server, stop, task): Host,
    runs: Arc<AtomicUsize>,
) {
    let child = json!({"sessionId":"child"});

    // Each tool with a rule rejects a call without a session, and does not
    // run for it.
    for tool in ["notes", "add_note", "count_notes"] {
        let missing = call_tool(&mut peer, &server, tool, json!({})).await;
        assert_rejected(&missing, SESSION_REQUIRED);
    }
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    // With a session each of them runs, also after a rejected call.
    let notes = call_tool(&mut peer, &server, "notes", child.clone()).await;
    assert_eq!(tool_text(&notes), "\"child\"", "{notes}");
    for tool in ["add_note", "count_notes"] {
        let ran = call_tool(&mut peer, &server, tool, child.clone()).await;
        assert_eq!(tool_text(&ran), "\"ran\"", "{tool}: {ran}");
    }
    assert_eq!(runs.load(Ordering::SeqCst), 3);

    // The second check added for `add_note` refuses one session.
    let other = json!({"sessionId":"other"});
    let refused = call_tool(&mut peer, &server, "add_note", other.clone()).await;
    assert_rejected(&refused, "sessionId rejected: not a session with notes");
    // The server's own check runs before the rule of the tool.
    let blocked = json!({"sessionId":"blocked"});
    let refused = call_tool(&mut peer, &server, "notes", blocked).await;
    assert_rejected(&refused, "sessionId rejected: blocked by the server");
    assert_eq!(runs.load(Ordering::SeqCst), 3);

    // A tool without a rule, and every other method, need no session.
    let help = call_tool(&mut peer, &server, "help", json!({})).await;
    assert_eq!(tool_text(&help), "\"ran\"", "{help}");
    let listed = attributed(&mut peer, &server, "tools/list", json!({}), json!({})).await;
    assert_eq!(
        listed["result"]["result"]["tools"].as_array().map(Vec::len),
        Some(4),
        "{listed}"
    );

    // A tool that is not registered has no rule. The server reports it as it
    // does without admission checks.
    let unknown = call_tool(&mut peer, &server, "missing", json!({})).await;
    assert_eq!(unknown.get("error"), None, "{unknown}");
    assert_eq!(
        unknown["result"]["error"],
        json!({"code":-32602,"message":"tool `missing` not found"})
    );

    let _sent = stop.send(());
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn builder_tools_state_their_own_admission_rules() {
    tokio::time::timeout(TIMEOUT, async {
        let server = |runs: &Arc<AtomicUsize>| {
            gated_tools::<Agent>(runs.clone()).with_admission(|request| {
                match request.session_id() {
                    Some(session_id) if session_id.0.as_ref() == "blocked" => {
                        Err(McpRejection::session_rejected("blocked by the server"))
                    }
                    _ => Ok(()),
                }
            })
        };
        let runs = Arc::new(AtomicUsize::new(0));
        tool_rules_decide_before_the_tool_runs(host(server(&runs)).await, runs).await;
        let runs = Arc::new(AtomicUsize::new(0));
        tool_rules_decide_before_the_tool_runs(host_v2(server(&runs)).await, runs).await;
    })
    .await
    .expect("builder admission test timed out");
}

/// A disabled tool has no rule either. The server reports the call as it
/// does without admission checks.
#[tokio::test]
async fn disabled_tool_gets_no_admission_rule() {
    tokio::time::timeout(TIMEOUT, async {
        let runs = Arc::new(AtomicUsize::new(0));
        let server = McpServer::<Agent>::builder("gated")
            .tool(Notes { runs: runs.clone() })
            .disable_tool("notes")
            .unwrap()
            .build();
        let (mut peer, server, stop, task) = host(server).await;
        let disabled = call_tool(&mut peer, &server, "notes", json!({})).await;
        assert_eq!(disabled.get("error"), None, "{disabled}");
        assert_eq!(
            disabled["result"]["error"],
            json!({"code":-32602,"message":"tool `notes` not found"})
        );
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        let _sent = stop.send(());
        task.await.unwrap().unwrap();
    })
    .await
    .expect("disabled tool test timed out");
}

/// An rmcp handler that counts the requests it runs, and reads the session
/// and tool call of a request.
struct SessionHandler {
    runs: Arc<AtomicUsize>,
}

impl SessionHandler {
    fn discovery(&self) -> DiscoverResult {
        self.runs.fetch_add(1, Ordering::SeqCst);
        DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            self.get_info(),
        )
    }

    fn tools(&self) -> ListToolsResult {
        self.runs.fetch_add(1, Ordering::SeqCst);
        ListToolsResult::with_all_items(Vec::new())
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Private)
    }

    fn tool(&self, context: &RequestContext<RoleServer>) -> CallToolResponse {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let attribution = McpRequestAttribution::of(context);
        CallToolResult::structured(json!({
            "sessionId":attribution.session_id(),
            "toolCallId":attribution.tool_call_id(),
        }))
        .into()
    }
}

impl ServerHandler for SessionHandler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
    fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<DiscoverResult, ErrorData>> + Send {
        std::future::ready(Ok(self.discovery()))
    }
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send {
        std::future::ready(Ok(self.tools()))
    }
    fn call_tool(
        &self,
        _request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + Send {
        std::future::ready(Ok(self.tool(&context)))
    }
}

/// A `from_rmcp` server whose check needs a session for discovery, for
/// `tools/list`, and for the tool `notes`. The SDK does not know the
/// handler's tools, so the rule for one tool is in the server's check.
fn handler_server<R: Role>(runs: &Arc<AtomicUsize>) -> McpServer<R> {
    let runs = runs.clone();
    McpServer::from_rmcp("sessions", move || SessionHandler { runs: runs.clone() }).with_admission(
        |request| match (request.method(), request.tool_name()) {
            ("tools/call", Some("notes")) | ("tools/list" | "server/discover", _) => {
                require_session(request)
            }
            _ => Ok(()),
        },
    )
}

async fn handler_runs_only_admitted_requests(
    (mut peer, server, stop, task): Host,
    runs: Arc<AtomicUsize>,
) {
    let structured = |response: &Value| response["result"]["result"]["structuredContent"].clone();
    let notes = json!({"name":"notes","arguments":{}});
    let requests = [
        ("tools/call", notes),
        ("tools/list", json!({})),
        ("server/discover", json!({})),
    ];

    for (method, inner) in &requests {
        let missing = attributed(&mut peer, &server, method, inner.clone(), json!({})).await;
        assert_rejected(&missing, SESSION_REQUIRED);
    }
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    let attribution = json!({"sessionId":"child","toolCallId":"call_1"});
    for (method, inner) in &requests {
        let present = attributed(
            &mut peer,
            &server,
            method,
            inner.clone(),
            attribution.clone(),
        );
        let present = present.await;
        assert_eq!(present.get("error"), None, "{method}: {present}");
    }
    assert_eq!(runs.load(Ordering::SeqCst), 3);

    // The handler reads the attribution of a request it runs.
    let seen = call_tool(&mut peer, &server, "notes", attribution.clone()).await;
    assert_eq!(structured(&seen), attribution);
    // A tool the check has no rule for runs without a session.
    let unattributed = call_tool(&mut peer, &server, "help", json!({})).await;
    assert_eq!(
        structured(&unattributed),
        json!({"sessionId":null,"toolCallId":null})
    );

    let _sent = stop.send(());
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn rmcp_handler_runs_only_admitted_requests_and_reads_their_attribution() {
    tokio::time::timeout(TIMEOUT, async {
        let runs = Arc::new(AtomicUsize::new(0));
        handler_runs_only_admitted_requests(host(handler_server(&runs)).await, runs).await;
        let runs = Arc::new(AtomicUsize::new(0));
        handler_runs_only_admitted_requests(host_v2(handler_server(&runs)).await, runs).await;
    })
    .await
    .expect("rmcp handler admission test timed out");
}

/// An rmcp handler whose `#[tool]` methods take the attribution as a
/// parameter.
struct ToolHandler {
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl ToolHandler {
    #[tool(description = "Returns the session and tool call of the request")]
    async fn read(&self, attribution: McpRequestAttribution) -> CallToolResult {
        CallToolResult::structured(json!({
            "sessionId":attribution.session_id(),
            "toolCallId":attribution.tool_call_id(),
        }))
    }

    /// rmcp's own extractor finds the attribution only where it is attached.
    #[tool(description = "Returns the session and tool call of the request")]
    fn from_extension(Extension(attribution): Extension<McpRequestAttribution>) -> CallToolResult {
        CallToolResult::structured(json!({
            "sessionId":attribution.session_id(),
            "toolCallId":attribution.tool_call_id(),
        }))
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

fn tool_server<R: Role>() -> McpServer<R> {
    McpServer::from_rmcp("tools", || ToolHandler {
        tool_router: ToolHandler::tool_router(),
    })
}

async fn tool_methods_take_the_attribution((mut peer, server, stop, task): Host) {
    let structured = |response: &Value| response["result"]["result"]["structuredContent"].clone();
    let attribution = json!({"sessionId":"child","toolCallId":"call_1"});
    for tool in ["read", "from_extension"] {
        let seen = call_tool(&mut peer, &server, tool, attribution.clone()).await;
        assert_eq!(structured(&seen), attribution, "{tool}");
        let unattributed = call_tool(&mut peer, &server, tool, json!({})).await;
        assert_eq!(
            structured(&unattributed),
            json!({"sessionId":null,"toolCallId":null}),
            "{tool}"
        );
    }

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

/// A standalone connection has no `mcp/message` request, so no admission
/// check runs and no attribution is attached. Tools and handlers run, and
/// read no session.
#[tokio::test]
async fn standalone_requests_run_without_admission_checks() -> Result<(), Error> {
    tokio::time::timeout(TIMEOUT, async {
        // The rules of builder tools are not evaluated.
        let runs = Arc::new(AtomicUsize::new(0));
        mcp::Client
            .builder()
            .connect_with(
                gated_tools::<mcp::Client>(runs.clone()),
                async |connection| {
                    for (tool, text) in [("notes", "\"\""), ("add_note", "\"ran\"")] {
                        let inner = json!({"name":tool,"arguments":{}});
                        let result = standalone_request(&connection, "tools/call", inner).await;
                        assert_eq!(result.unwrap()["content"][0]["text"], text, "{tool}");
                    }
                    Ok(())
                },
            )
            .await?;
        assert_eq!(runs.load(Ordering::SeqCst), 2);

        // Nor is the check of a `from_rmcp` server, and its handler reads no
        // attribution.
        let runs = Arc::new(AtomicUsize::new(0));
        mcp::Client
            .builder()
            .connect_with(handler_server::<mcp::Client>(&runs), async |connection| {
                let notes = json!({"name":"notes","arguments":{}});
                let result = standalone_request(&connection, "tools/call", notes).await;
                assert_eq!(
                    result.unwrap()["structuredContent"],
                    json!({"sessionId":null,"toolCallId":null})
                );
                let listed = standalone_request(&connection, "tools/list", json!({})).await;
                assert_eq!(listed.unwrap()["tools"], json!([]));
                Ok(())
            })
            .await?;
        assert_eq!(runs.load(Ordering::SeqCst), 2);

        // A `#[tool]` parameter is an attribution without a session. rmcp's
        // `Extension` extractor fails, because nothing is attached.
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
    .expect("standalone admission test timed out")
}
