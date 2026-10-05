//! Cancellation of real mutable function tools shared by native providers.
#![cfg(feature = "unstable_mcp_over_acp")]

use agent_client_protocol::{
    Agent, Channel, Client, Error, RawJsonRpcMessage, RunWithConnectionTo, TransportFrame,
    mcp_server::{McpConnectionTo, McpServer, McpTool, tool_fn_mut},
};
use agent_client_protocol_rmcp::McpServerExt;
use futures::{StreamExt, channel::mpsc, future::poll_fn};
use rmcp::{
    ErrorData, ServerHandler,
    model::{ServerCapabilities, ServerConfig, SubscriptionFilter},
    service::{SubscriptionContext, SubscriptionSink},
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;

#[derive(Default)]
struct State {
    entered: Mutex<Vec<u32>>,
    dropped: Mutex<Vec<u32>>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct Args {
    id: u32,
}

struct UserDrop {
    id: u32,
    state: Arc<State>,
}

impl Drop for UserDrop {
    fn drop(&mut self) {
        self.state.dropped.lock().unwrap().push(self.id);
    }
}

/// Separate builder registrations delegate to the same actual tool/runner.
/// The first-poll marker establishes admission, not just handler entry.
struct SharedTool<T> {
    tool: Arc<T>,
    polled: mpsc::UnboundedSender<()>,
}

impl<T: McpTool<Agent>> McpTool<Agent> for SharedTool<T> {
    type Input = T::Input;
    type Output = T::Output;

    fn name(&self) -> String {
        self.tool.name()
    }

    fn description(&self) -> String {
        self.tool.description()
    }

    async fn call_tool(
        &self,
        args: Self::Input,
        cx: McpConnectionTo<Agent>,
    ) -> Result<Self::Output, Error> {
        let call = self.tool.call_tool(args, cx);
        futures::pin_mut!(call);
        let mut marked = false;
        poll_fn(|cx| {
            let result = call.as_mut().poll(cx);
            if !marked {
                // Only two calls are admitted in these tests, far below the
                // existing 128-slot capacity. A pending first poll has therefore
                // completed send() and reached the result receiver.
                assert!(result.is_pending());
                self.polled.unbounded_send(()).unwrap();
                marked = true;
            }
            result
        })
        .await
    }
}

struct Subscription {
    started: Mutex<Option<oneshot::Sender<SubscriptionSink>>>,
    dropped: Arc<Mutex<bool>>,
}

struct SubscriptionDrop(Arc<Mutex<bool>>);

impl Drop for SubscriptionDrop {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = true;
    }
}

impl ServerHandler for Subscription {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
    }

    fn accepted_subscription_filter(
        &self,
        filter: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        Some(filter.clone())
    }

    async fn listen(&self, cx: SubscriptionContext) -> Result<(), ErrorData> {
        let _drop = SubscriptionDrop(self.dropped.clone());
        self.started
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .send(cx.sink().clone())
            .unwrap();
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
        panic!("expected a single frame")
    };
    serde_json::to_value(message).unwrap()
}

async fn response(peer: &mut Channel, id: &str) -> Value {
    let value = receive(peer).await;
    assert_eq!(value["id"], id, "unexpected frame: {value}");
    value
}

fn request(
    peer: &Channel,
    server: &Value,
    outer: &str,
    logical: &str,
    method: &str,
    mut params: Value,
) {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{}
    });
    send(
        peer,
        json!({"jsonrpc":"2.0","id":outer,"method":"mcp/message","params":{
            "serverId":server,"requestId":logical,"method":method,"params":params
        }}),
    );
}

async fn setup(peer: &mut Channel, session: &str) -> Value {
    let setup = receive(peer).await;
    assert_eq!(setup["method"], "session/new");
    send(
        peer,
        json!({"jsonrpc":"2.0","id":setup["id"],"result":{"sessionId":session}}),
    );
    setup["params"]["mcpServers"].clone()
}

async fn queued_cancellation(end_scope: bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let state = Arc::new(State::default());
        let (polled, mut polls) = mpsc::unbounded();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let operation = {
            let state = state.clone();
            let mut started = Some(started_tx);
            let mut release = Some(release_rx);
            // Mutable captured state is borrowed across suspension by the
            // actual async closure, not an artificial service cleanup future.
            async move |args: Args, _cx: McpConnectionTo<Agent>| {
                let _drop = UserDrop {
                    id: args.id,
                    state: state.clone(),
                };
                state.entered.lock().unwrap().push(args.id);
                if args.id == 0 {
                    started.take().unwrap().send(()).unwrap();
                    release.take().unwrap().await.unwrap();
                }
                Ok::<_, Error>(args.id)
            }
        };
        let (tool, runner) = tool_fn_mut(
            "shared",
            "serialized shared tool",
            operation,
            agent_client_protocol::tool_fn_mut!(),
        );
        let tool = Arc::new(tool);
        let server_a = McpServer::builder("a")
            .tool(SharedTool {
                tool: tool.clone(),
                polled: polled.clone(),
            })
            .build();
        let server_b = McpServer::builder("b")
            .tool(SharedTool { tool, polled })
            .build();
        let (sub_tx, sub_rx) = oneshot::channel();
        let subscription_dropped = Arc::new(Mutex::new(false));
        let subscription = McpServer::from_rmcp("subscription", {
            let started = Arc::new(Mutex::new(Some(sub_tx)));
            let dropped = subscription_dropped.clone();
            move || Subscription {
                started: Mutex::new(started.lock().unwrap().take()),
                dropped: dropped.clone(),
            }
        });
        let (transport, mut peer) = Channel::duplex();
        let (stop_a, stopped_a) = oneshot::channel();
        let (stop_b, stopped_b) = oneshot::channel();
        let mut stop_b = Some(stop_b);
        let (returned_b, returned_b_rx) = oneshot::channel();
        let (finish, finished) = oneshot::channel();
        let task = tokio::spawn(Client.builder().connect_with(transport, async move |cx| {
            // The shared runner belongs to the connection, not either of the
            // independent registrations that happen to delegate to it.
            cx.spawn(runner.run_with_connection_to(cx.clone()))?;
            let a = cx
                .build_session_cwd()?
                .with_mcp_server(server_a)?
                .with_mcp_server(subscription)?
                .block_task()
                .run_until(async |_session| {
                    stopped_a.await.unwrap();
                    Ok(())
                });
            let b = async {
                cx.build_session_cwd()?
                    .with_mcp_server(server_b)?
                    .block_task()
                    .run_until(async |_session| {
                        stopped_b.await.unwrap();
                        Ok(())
                    })
                    .await?;
                returned_b.send(()).unwrap();
                finished.await.unwrap();
                Ok::<_, Error>(())
            };
            futures::try_join!(a, b)?;
            Ok(())
        }));
        let a = setup(&mut peer, "a").await;
        let b = setup(&mut peer, "b").await;
        let server_a = &a[0]["serverId"];
        let server_b = &b[0]["serverId"];
        request(
            &peer,
            &a[1]["serverId"],
            "listen",
            "listen",
            "subscriptions/listen",
            json!({"notifications":{"toolsListChanged":true}}),
        );
        let subscription = sub_rx.await.unwrap();
        assert_eq!(
            receive(&mut peer).await["params"]["method"],
            "notifications/subscriptions/acknowledged"
        );
        request(
            &peer,
            server_a,
            "a-call",
            "active",
            "tools/call",
            json!({"name":"shared","arguments":{"id":0}}),
        );
        assert_eq!(polls.next().await, Some(()));
        started_rx.await.unwrap();
        request(
            &peer,
            server_b,
            "b-call",
            "queued",
            "tools/call",
            json!({"name":"shared","arguments":{"id":1}}),
        );
        assert_eq!(polls.next().await, Some(()));
        assert_eq!(*state.entered.lock().unwrap(), [0]);
        assert!(state.dropped.lock().unwrap().is_empty());

        if end_scope {
            stop_b.take().unwrap().send(()).unwrap();
        } else {
            send(
                &peer,
                json!({"jsonrpc":"2.0","method":"$/cancel_request",
                "params":{"requestId":"b-call"}}),
            );
        }
        let cancelled = response(&mut peer, "b-call").await;
        assert_eq!(
            cancelled["error"]["code"],
            if end_scope { -33001 } else { -32800 },
            "{cancelled}"
        );
        assert!(
            state.dropped.lock().unwrap().is_empty(),
            "cancellation interrupted A"
        );
        assert_eq!(*state.entered.lock().unwrap(), [0], "B executed");

        if end_scope {
            // Must return while A's actual user future is still suspended.
            returned_b_rx.await.unwrap();
        } else {
            // Same provider and logical ID: tools/list bypasses serialized
            // invocation, so success proves ID release without completing A.
            request(
                &peer,
                server_b,
                "reuse-b",
                "queued",
                "tools/list",
                json!({}),
            );
            assert!(response(&mut peer, "reuse-b").await["result"]["result"]["tools"].is_array());
            stop_b.take().unwrap().send(()).unwrap();
            returned_b_rx.await.unwrap();
        }
        assert_eq!(*state.entered.lock().unwrap(), [0]);
        assert!(state.dropped.lock().unwrap().is_empty());
        assert!(!*subscription_dropped.lock().unwrap());
        subscription.notify_tool_list_changed().await.unwrap();
        let notification = receive(&mut peer).await;
        assert_eq!(notification["params"]["requestId"], "listen");
        assert_eq!(
            notification["params"]["method"],
            "notifications/tools/list_changed"
        );
        request(
            &peer,
            server_a,
            "a-list",
            "admission",
            "tools/list",
            json!({}),
        );
        assert!(response(&mut peer, "a-list").await["result"]["result"]["tools"].is_array());

        release_tx.send(()).unwrap();
        assert!(response(&mut peer, "a-call").await["result"]["result"]["content"].is_array());
        assert_eq!(*state.dropped.lock().unwrap(), [0]);
        request(
            &peer,
            server_a,
            "a-next",
            "active",
            "tools/call",
            json!({"name":"shared","arguments":{"id":2}}),
        );
        assert_eq!(polls.next().await, Some(()));
        assert!(response(&mut peer, "a-next").await["result"]["result"]["content"].is_array());
        assert_eq!(*state.entered.lock().unwrap(), [0, 2]);
        stop_a.send(()).unwrap();
        finish.send(()).unwrap();
        task.await.unwrap().unwrap();
        assert!(*subscription_dropped.lock().unwrap());
    })
    .await
    .expect("queued tool cancellation timed out");
}

#[tokio::test]
async fn cancelled_queued_call_releases_id_without_interrupting_shared_mutable_call() {
    queued_cancellation(false).await;
}

#[tokio::test]
async fn ending_queued_provider_scope_does_not_wait_for_other_providers_mutable_call() {
    queued_cancellation(true).await;
}
