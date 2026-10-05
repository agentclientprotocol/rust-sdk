#![cfg(feature = "unstable_mcp_over_acp")]

use agent_client_protocol::{
    Agent, Channel, Client, ConnectTo, ConnectionDriver, DynConnectTo, Error, NullRun,
    TransportFrame,
    mcp_server::{
        McpConnectionTo, McpOutcome, McpRequest, McpRequestContext, McpServer, McpServerConnect,
        McpService,
    },
    role,
};
use futures::{StreamExt, future::BoxFuture};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::oneshot;

struct Service(Value);

impl McpService<Agent> for Service {
    fn execute(
        &self,
        request: McpRequest,
        _: McpRequestContext<Agent>,
    ) -> BoxFuture<'static, Result<McpOutcome, Error>> {
        assert_eq!(request.method, "server/discover");
        let result = self.0.clone();
        Box::pin(async move { Ok(McpOutcome::Result(result)) })
    }
}

struct Connector(Value);

impl McpServerConnect<Agent> for Connector {
    fn name(&self) -> String {
        "discovery".into()
    }

    fn connect(&self, _: McpConnectionTo<Agent>) -> DynConnectTo<role::mcp::Client> {
        DynConnectTo::new(Backend(self.0.clone()))
    }
}

struct Backend(Value);

impl ConnectTo<role::mcp::Client> for Backend {
    fn connect_to(
        self,
        _: impl ConnectTo<role::mcp::Server>,
    ) -> impl std::future::Future<Output = Result<(), Error>> {
        std::future::poll_fn(|_| panic!("request-scoped factory must use its driver"))
    }

    fn into_channel_and_future(self) -> (Channel, Option<ConnectionDriver>) {
        let (client, mut server) = Channel::duplex();
        let driver = ConnectionDriver::new(async move {
            let request = receive(&mut server).await;
            assert_eq!(request["method"], "server/discover");
            send(
                &server,
                json!({"jsonrpc":"2.0","id":request["id"],"result":self.0}),
            );
            Ok(())
        });
        (client, Some(driver))
    }
}

fn send(peer: &Channel, message: Value) {
    peer.tx
        .unbounded_send(TransportFrame::Single(
            serde_json::from_value(message).unwrap(),
        ))
        .unwrap();
}

async fn receive(peer: &mut Channel) -> Value {
    let TransportFrame::Single(message) = peer.rx.next().await.unwrap() else {
        panic!("single frame")
    };
    serde_json::to_value(message).unwrap()
}

async fn discover(native: bool, backend_result: Value) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (transport, mut peer) = Channel::duplex();
        let (stop_tx, stop_rx) = oneshot::channel();
        let server = if native {
            McpServer::new_service("discovery", Service(backend_result), NullRun)
        } else {
            McpServer::new(Connector(backend_result), NullRun)
        };
        let client = tokio::spawn(Client.builder().connect_with(transport, async move |cx| {
            cx.build_session_cwd()?
                .with_mcp_server(server)?
                .block_task()
                .run_until(async |_session| {
                    stop_rx.await.unwrap();
                    Ok(())
                })
                .await
        }));
        let setup = receive(&mut peer).await;
        let server_id = setup["params"]["mcpServers"][0]["serverId"].clone();
        send(
            &peer,
            json!({"jsonrpc":"2.0","id":setup["id"],"result":{"sessionId":"discovery"}}),
        );
        send(
            &peer,
            json!({"jsonrpc":"2.0","id":"discover","method":"mcp/message","params":{
                "serverId":server_id,"requestId":"discover","method":"server/discover","params":{
                    "_meta":{
                        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities":{}
                    }
                }
            }}),
        );
        let response = receive(&mut peer).await;
        assert_eq!(response["id"], "discover");
        let _ = stop_tx.send(());
        client.await.unwrap().unwrap();
        response
    })
    .await
    .expect("MCP discovery timed out")
}

#[tokio::test]
async fn native_and_factory_discovery_project_binding_revision_not_backend_version() {
    for native in [true, false] {
        let original = json!({
            "supportedVersions":["2024-11-05","2025-03-26","2025-11-25","2026-07-28"],
            "serverInfo":{"name":"backend","version":"9.7.1"},
            "capabilities":{"tools":{}},
            "_meta":{"vendor/opaque":[null,42]},
            "extension":{"preserved":true}
        });
        let mut expected = original.clone();
        expected["supportedVersions"] = json!(["2026-07-28"]);
        assert_eq!(
            discover(native, original).await["result"],
            json!({"result":expected})
        );
    }
}

#[tokio::test]
async fn native_and_factory_discovery_distinguish_unsupported_from_malformed() {
    for native in [true, false] {
        let unsupported = discover(native, json!({"supportedVersions":["2025-11-25"]})).await;
        assert_eq!(
            unsupported["result"],
            json!({"error":{
                "code":-32022,"message":"Unsupported protocol version",
                "data":{"requested":"2026-07-28","supported":["2025-11-25"]}
            }})
        );
        for malformed in [
            Value::Null,
            json!({}),
            json!({"supportedVersions":"2026-07-28"}),
            json!({"supportedVersions":["2026-07-28",null]}),
        ] {
            assert_eq!(discover(native, malformed).await["error"]["code"], -33002);
        }
    }
}
