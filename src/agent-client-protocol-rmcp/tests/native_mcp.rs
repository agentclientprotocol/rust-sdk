//! Real rmcp service lifecycle through native ACP carriers and raw Channels.
#![cfg(feature = "unstable_mcp_over_acp")]

use agent_client_protocol::{
    Agent, Channel, Client, Error, RawJsonRpcMessage, RunWithConnectionTo, TransportFrame,
    mcp_server::{McpConnectionTo, McpServer},
};
use agent_client_protocol_rmcp::McpServerExt;
use futures::StreamExt;
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, InputRequiredResult,
        ServerCapabilities, ServerConfig, SubscriptionFilter,
    },
    service::{RequestContext, SubscriptionContext},
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
    let task = tokio::spawn(Client.builder().connect_with(transport, async move |cx| {
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
                    agent_client_protocol_rmcp::tool_fn_mut!(),
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
                    agent_client_protocol_rmcp::tool_fn!(),
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
