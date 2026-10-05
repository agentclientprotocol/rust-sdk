#![cfg(feature = "unstable_mcp_over_acp")]

use agent_client_protocol::{
    Agent, Channel, Client, ConnectTo, ConnectionDriver, DynConnectTo, Error, NullRun,
    RawJsonRpcMessage, TransportFrame,
    mcp_server::{McpConnectionTo, McpServer, McpServerConnect},
    role,
};
use futures::{FutureExt, StreamExt};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot;

#[derive(Clone, Copy)]
enum Mode {
    Cooperative,
    Opaque,
    NoDriver,
    Broken,
    Reverse,
    FinishError,
}
struct Connector {
    mode: Mode,
    outcome: Value,
    cleanup_started: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
    finished: Arc<AtomicBool>,
}
impl McpServerConnect<Agent> for Connector {
    fn name(&self) -> String {
        "factory".into()
    }
    fn connect(&self, cx: McpConnectionTo<Agent>) -> DynConnectTo<role::mcp::Client> {
        assert_eq!(cx.request_id().unwrap().to_string(), "logical");
        DynConnectTo::new(Backend {
            mode: self.mode,
            outcome: self.outcome.clone(),
            cleanup_started: self.cleanup_started.lock().unwrap().take(),
            release: self.release.lock().unwrap().take(),
            finished: self.finished.clone(),
        })
    }
}
struct Backend {
    mode: Mode,
    outcome: Value,
    cleanup_started: Option<oneshot::Sender<()>>,
    release: Option<oneshot::Receiver<()>>,
    finished: Arc<AtomicBool>,
}
impl ConnectTo<role::mcp::Client> for Backend {
    fn connect_to(
        self,
        _: impl ConnectTo<role::mcp::Server>,
    ) -> impl std::future::Future<Output = Result<(), Error>> {
        std::future::poll_fn(|_| {
            panic!("native factory must preserve into_channel_and_future override")
        })
    }
    fn into_channel_and_future(self) -> (Channel, Option<ConnectionDriver>) {
        let mode = self.mode;
        let (client, mut server) = Channel::duplex();
        let (finish_tx, finish_rx) = oneshot::channel();
        let work = async move {
            let TransportFrame::Single(message) = server.rx.next().await.expect("MCP request")
            else {
                panic!("single frame")
            };
            let request = serde_json::to_value(message).unwrap();
            assert_eq!(request["id"], "logical");
            assert_eq!(request["method"], "opaque");
            assert_eq!(
                request["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
                "2026-07-28"
            );
            if matches!(self.mode, Mode::Broken) {
                return Err(Error::internal_error().data("backend broke"));
            }
            let mut response = self.outcome;
            response["jsonrpc"] = json!("2.0");
            response["id"] = request["id"].clone();
            if matches!(self.mode, Mode::Reverse) {
                response = json!({"jsonrpc":"2.0","id":"reverse","method":"sampling/createMessage","params":{}});
            }
            let raw: RawJsonRpcMessage = serde_json::from_value(response).unwrap();
            server
                .tx
                .unbounded_send(TransportFrame::Single(raw))
                .unwrap();
            if matches!(self.mode, Mode::Cooperative | Mode::FinishError) {
                finish_rx
                    .await
                    .expect("binding must request graceful finish");
                if let Some(started) = self.cleanup_started {
                    let _sent = started.send(());
                }
                if let Some(release) = self.release {
                    release.await.unwrap();
                }
                self.finished.store(true, Ordering::SeqCst);
                if matches!(self.mode, Mode::FinishError) {
                    return Err(Error::internal_error().data("flush failed"));
                }
                Ok(())
            } else {
                self.finished.store(true, Ordering::SeqCst);
                // Exit immediately after accepting output. The binding must
                // drain that terminal response even though the driver errors.
                Err(Error::internal_error().data("driver exited after output"))
            }
        };
        if matches!(mode, Mode::NoDriver) {
            // An externally driven endpoint legitimately has no owned driver.
            tokio::spawn(work);
            (client, None)
        } else if matches!(mode, Mode::Cooperative | Mode::FinishError) {
            (
                client,
                Some(
                    ConnectionDriver::with_finish(work, move || {
                        let _sent = finish_tx.send(());
                    })
                    .map_future(|work| {
                        work.inspect(|result| tracing::trace!(?result, "factory driver finished"))
                    }),
                ),
            )
        } else {
            (client, Some(ConnectionDriver::new(work)))
        }
    }
}
fn send(peer: &Channel, value: Value) {
    peer.tx
        .unbounded_send(TransportFrame::Single(
            serde_json::from_value(value).unwrap(),
        ))
        .unwrap();
}
async fn receive(peer: &mut Channel) -> Value {
    let TransportFrame::Single(message) = peer.rx.next().await.expect("frame") else {
        panic!("single frame")
    };
    serde_json::to_value(message).unwrap()
}
fn request(peer: &Channel, server: &Value, outer: &str) {
    send(
        peer,
        json!({"jsonrpc":"2.0","id":outer,"method":"mcp/message","params":{
            "serverId":server,"requestId":"logical","method":"opaque","params":{"_meta":{
                "io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}
            }}
        }}),
    );
}
async fn run(mode: Mode, outcome: Value) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async move {
        let (transport, mut peer) = Channel::duplex();
        let (stop_tx, stop_rx) = oneshot::channel();
        let (cleanup_tx, cleanup_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let connector = Connector {
            mode,
            outcome,
            cleanup_started: Mutex::new(Some(cleanup_tx)),
            release: Mutex::new(Some(release_rx)),
            finished: finished.clone(),
        };
        let client = tokio::spawn(Client.builder().connect_with(transport, async move |cx| {
            cx.build_session_cwd()?
                .with_mcp_server(McpServer::new(connector, NullRun))?
                .block_task()
                .run_until(async |_session| {
                    let _stop = stop_rx.await;
                    Ok(())
                })
                .await?;
            Ok(())
        }));
        let setup = receive(&mut peer).await;
        let server = setup["params"]["mcpServers"][0]["serverId"].clone();
        send(
            &peer,
            json!({"jsonrpc":"2.0","id":setup["id"],"result":{"sessionId":"factory"}}),
        );
        request(&peer, &server, "original");
        if matches!(mode, Mode::Cooperative | Mode::FinishError) {
            cleanup_rx.await.unwrap();
            assert!(!finished.load(Ordering::SeqCst));
            request(&peer, &server, "duplicate");
            let duplicate = receive(&mut peer).await;
            assert_eq!(duplicate["id"], "duplicate");
            assert_eq!(
                duplicate["error"]["code"], -32602,
                "ID released before cleanup"
            );
            let _sent = release_tx.send(());
        }
        let response = receive(&mut peer).await;
        assert_eq!(response["id"], "original");
        if !matches!(mode, Mode::Broken | Mode::Reverse) {
            assert!(finished.load(Ordering::SeqCst));
        }
        let _sent = stop_tx.send(());
        client.await.unwrap().unwrap();
        response
    })
    .await
    .expect("factory timed out")
}

#[tokio::test]
async fn cooperative_factory_cleanup_precedes_response_and_logical_id_release() {
    let response = run(Mode::Cooperative, json!({"result":null})).await;
    assert_eq!(response["result"], json!({"result":null}));
}
#[tokio::test]
async fn optional_driver_and_accepted_terminal_output_are_preserved() {
    for mode in [Mode::Opaque, Mode::NoDriver] {
        let response = run(mode, json!({"result":{"opaque":true}})).await;
        assert_eq!(response["result"], json!({"result":{"opaque":true}}));
    }
}
#[tokio::test]
async fn factory_errors_keep_mcp_codes_extensions_and_absent_or_null_data() {
    for data in [None, Some(Value::Null), Some(json!({"upstream":"failed"}))] {
        let mut error =
            json!({"code":-32022,"message":"raw peer error","extension":{"retry":false}});
        if let Some(data) = data {
            error["data"] = data;
        }
        let response = run(Mode::Opaque, json!({"error":error})).await;
        assert_eq!(response["result"], json!({"error":error}));
    }
}
#[tokio::test]
async fn backend_and_graceful_finish_failures_are_outer_errors_not_mcp_outcomes() {
    for mode in [Mode::Broken, Mode::Reverse, Mode::FinishError] {
        let response = run(mode, json!({"result":null})).await;
        assert_eq!(response["error"]["code"], -33002);
        assert!(response.get("result").is_none());
    }
}
