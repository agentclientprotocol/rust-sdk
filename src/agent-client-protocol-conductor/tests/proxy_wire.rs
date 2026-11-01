//! Exact wire traffic of proxy chains.
//!
//! These tests drive a conductor from raw JSON lines on every side they
//! observe, so they check the bytes a peer sends and receives:
//!
//! - The results and errors of wrapped requests reach the original requester
//!   unchanged, in both directions, as the proxy chains RFD requires.
//! - `proxy/initialize` and `proxy/successor` keep their wire format.

use std::{io, time::Duration};

use agent_client_protocol::{Client, ConnectTo, DynConnectTo, Proxy, Role};
use agent_client_protocol_conductor::{
    ConductorImpl, InstantiatedChain, SelectedInitializeRequest,
};
use expect_test::{Expect, expect};
use futures::{SinkExt as _, StreamExt as _, channel::mpsc};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(5);

/// The test's side of a raw JSON-lines connection.
struct Endpoint {
    name: &'static str,
    tx: mpsc::UnboundedSender<io::Result<String>>,
    rx: mpsc::UnboundedReceiver<String>,
}

impl Endpoint {
    /// Send `line` exactly as written.
    fn send(&self, line: &str) {
        self.tx
            .unbounded_send(Ok(line.to_owned()))
            .unwrap_or_else(|_| panic!("{} is no longer read", self.name));
    }

    /// The next line the peer wrote.
    async fn recv(&mut self) -> String {
        tokio::time::timeout(TIMEOUT, self.rx.next())
            .await
            .unwrap_or_else(|_| panic!("{} should receive a line in time", self.name))
            .unwrap_or_else(|| panic!("{} closed", self.name))
    }

    /// The next line the peer wrote, parsed.
    async fn recv_json(&mut self) -> (String, Value) {
        let line = self.recv().await;
        let value = serde_json::from_str(&line).expect("peers write JSON");
        (line, value)
    }
}

/// A component whose traffic the test reads and writes as raw lines.
fn raw_component<R: Role>(name: &'static str) -> (impl ConnectTo<R> + 'static, Endpoint) {
    let (tx, incoming) = mpsc::unbounded::<io::Result<String>>();
    let (outgoing, rx) = mpsc::unbounded::<String>();
    let lines =
        agent_client_protocol::Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
    (lines, Endpoint { name, tx, rx })
}

/// Start a conductor in agent mode over `proxies` and a raw agent, and return
/// the client and agent endpoints.
fn start_chain(
    proxies: Vec<DynConnectTo<agent_client_protocol::Conductor>>,
) -> (Endpoint, Endpoint, tokio::task::JoinHandle<()>) {
    let (agent, agent_end) = raw_component::<Client>("agent");
    let conductor = ConductorImpl::new_agent(
        "proxy-wire",
        move |request: SelectedInitializeRequest| async move {
            Ok(InstantiatedChain::new(
                request,
                proxies,
                DynConnectTo::new(agent),
            ))
        },
    );
    let (client, client_end) = raw_component::<agent_client_protocol::Agent>("client");
    let task = tokio::spawn(async move {
        drop(conductor.connect_to(client).await);
    });
    (client_end, agent_end, task)
}

fn initialize_lines(version: u16) -> (String, String) {
    let request = match version {
        1 => json!({ "protocolVersion": 1, "_unknownField": true }),
        _ => json!({
            "protocolVersion": version,
            "info": { "name": "raw-client", "version": "1.0.0" },
            "_unknownField": true,
        }),
    };
    let response = match version {
        1 => json!({
            "protocolVersion": 1,
            "agentCapabilities": {},
            "authMethods": [],
        }),
        _ => json!({
            "protocolVersion": version,
            "info": { "name": "raw-agent", "version": "1.0.0" },
        }),
    };
    (request.to_string(), response.to_string())
}

/// Initialize the chain from the client, answering as the raw agent.
async fn initialize(client: &mut Endpoint, agent: &mut Endpoint, version: u16) {
    let (params, result) = initialize_lines(version);
    client.send(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{params}}}"#
    ));
    let (_, request) = agent.recv_json().await;
    assert_eq!(request["method"], "initialize", "{request}");
    agent.send(&format!(
        r#"{{"jsonrpc":"2.0","id":{},"result":{result}}}"#,
        request["id"]
    ));
    let (line, response) = client.recv_json().await;
    assert_eq!(response["id"], 1, "{line}");
    assert!(response.get("result").is_some(), "{line}");
}

/// Errors and results whose bytes must survive the chain unchanged. Each is
/// written in serde_json's compact form, so a byte comparison of the forwarded
/// line is meaningful.
const OUTCOMES: &[&str] = &[
    r#""error":{"code":-32042,"message":"custom failure","data":{"nested":{"list":[1,"two",null,true,-0.5]},"text":"é ✓ \"quoted\"","_meta":{"k":"v"}}}"#,
    r#""error":{"code":4242,"message":"positive custom code","data":null}"#,
    r#""error":{"code":-32603,"message":"no data"}"#,
    r#""error":{"code":-32601,"message":"Method not found","data":"_vendor/method"}"#,
    r#""result":{"zeta":1,"alpha":[{"x":null}],"_meta":{"k":"v"},"unknownField":"kept"}"#,
    r#""result":null"#,
];

/// Send requests from `from` to `to` through the chain and answer each with
/// one of [`OUTCOMES`]; every outcome must arrive byte-for-byte.
async fn assert_outcomes_pass_through(from: &mut Endpoint, to: &mut Endpoint, method: &str) {
    for (index, outcome) in OUTCOMES.iter().enumerate() {
        let id = format!(r#""{}-{index}""#, from.name);
        from.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{{"index":{index}}}}}"#
        ));
        let (line, request) = to.recv_json().await;
        assert_eq!(request["method"], method, "{line}");
        assert_eq!(request["params"], json!({ "index": index }), "{line}");
        to.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{},{outcome}}}"#,
            request["id"]
        ));
        let (line, response) = from.recv_json().await;
        assert_eq!(response["id"].to_string(), id, "{line}");
        assert!(
            line.contains(outcome),
            "{} expected the outcome unchanged.\n  sent:     {outcome}\n  received: {line}",
            from.name,
        );
    }
}

async fn assert_chain_passes_outcomes_through(
    proxies: Vec<DynConnectTo<agent_client_protocol::Conductor>>,
    version: u16,
) {
    let (mut client, mut agent, task) = start_chain(proxies);
    initialize(&mut client, &mut agent, version).await;

    // Client -> agent: the agent's results and errors reach the client.
    assert_outcomes_pass_through(&mut client, &mut agent, "_vendor/forward").await;
    // Agent -> client: the client's results and errors reach the agent.
    assert_outcomes_pass_through(&mut agent, &mut client, "_vendor/backward").await;

    // Proxies read the session ID from a `session/new` result to route the
    // session's updates, but return the result unchanged even when it has none.
    for (id, outcome) in [
        ("new-1", r#""result":{"notASessionId":true}"#),
        (
            "new-2",
            r#""error":{"code":-32042,"message":"no session","data":{"why":"custom"}}"#,
        ),
        ("new-3", r#""result":{"sessionId":"s-1","_meta":{"k":"v"}}"#),
    ] {
        client.send(&format!(
            r#"{{"jsonrpc":"2.0","id":"{id}","method":"session/new","params":{{"cwd":"/tmp","mcpServers":[]}}}}"#
        ));
        let (line, request) = agent.recv_json().await;
        assert_eq!(request["method"], "session/new", "{line}");
        agent.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{},{outcome}}}"#,
            request["id"]
        ));
        let line = client.recv().await;
        assert!(
            line.contains(outcome),
            "expected the session/new outcome unchanged.\n  sent:     {outcome}\n  received: {line}",
        );
    }

    // Once the session exists, the proxies route its requests through their
    // session handlers, which return outcomes unchanged as well.
    let error = r#""error":{"code":-32042,"message":"declined","data":{"sessionId":"s-1"}}"#;
    agent.send(r#"{"jsonrpc":"2.0","id":"in-session","method":"_vendor/session","params":{"sessionId":"s-1"}}"#);
    let (line, request) = client.recv_json().await;
    assert_eq!(request["params"]["sessionId"], "s-1", "{line}");
    client.send(&format!(
        r#"{{"jsonrpc":"2.0","id":{},{error}}}"#,
        request["id"]
    ));
    let line = agent.recv().await;
    assert!(line.contains(error), "expected the error unchanged: {line}");

    task.abort();
}

/// Through two v1 proxies, results and errors of wrapped requests are
/// returned unchanged in both directions.
#[tokio::test(flavor = "current_thread")]
async fn v1_chain_returns_results_and_errors_unchanged() {
    assert_chain_passes_outcomes_through(
        vec![
            DynConnectTo::new(Proxy.v1().name("first")),
            DynConnectTo::new(Proxy.v1().name("second")),
        ],
        1,
    )
    .await;
}

/// The same through two draft v2 proxies.
#[cfg(feature = "unstable_protocol_v2")]
#[tokio::test(flavor = "current_thread")]
async fn v2_chain_returns_results_and_errors_unchanged() {
    assert_chain_passes_outcomes_through(
        vec![
            DynConnectTo::new(Proxy.v2().name("first")),
            DynConnectTo::new(Proxy.v2().name("second")),
        ],
        2,
    )
    .await;
}

/// Replace the conductor's generated request IDs so transcripts are stable.
fn normalize(line: &str) -> String {
    let uuid =
        regex::Regex::new(r#""[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}""#)
            .expect("valid regex");
    uuid.replace_all(line, r#""<generated>""#).into_owned()
}

/// Records the lines one component received, labelled.
#[derive(Default)]
struct Transcript(Vec<String>);

impl Transcript {
    fn record(&mut self, label: &str, line: &str) {
        self.0.push(format!("{label}: {}", normalize(line)));
    }

    fn check(&self, expected: &Expect) {
        expected.assert_eq(&(self.0.join("\n") + "\n"));
    }
}

/// Run a chain with one raw proxy and record the proxy-chain messages on the
/// wire: `proxy/initialize` and its response, and `proxy/successor` requests,
/// notifications and responses in both directions.
async fn proxy_transcript(version: u16) -> Transcript {
    let (proxy, mut proxy_end) = raw_component::<agent_client_protocol::Conductor>("proxy");
    let (mut client, mut agent, task) = start_chain(vec![DynConnectTo::new(proxy)]);
    let mut transcript = Transcript::default();
    let (params, result) = initialize_lines(version);

    // `initialize` reaches the proxy as `proxy/initialize`.
    client.send(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{params}}}"#
    ));
    let (line, init) = proxy_end.recv_json().await;
    transcript.record("conductor -> proxy", &line);

    // The proxy forwards `initialize` to its successor.
    proxy_end.send(&format!(
        r#"{{"jsonrpc":"2.0","id":"p-init","method":"proxy/successor","params":{{"method":"initialize","params":{params},"_meta":{{"envelope":true}}}}}}"#
    ));
    let (line, request) = agent.recv_json().await;
    transcript.record("conductor -> agent", &line);
    agent.send(&format!(
        r#"{{"jsonrpc":"2.0","id":{},"result":{result}}}"#,
        request["id"]
    ));
    let line = proxy_end.recv().await;
    transcript.record("conductor -> proxy", &line);

    // The proxy answers `proxy/initialize`.
    proxy_end.send(&format!(
        r#"{{"jsonrpc":"2.0","id":{},"result":{result}}}"#,
        init["id"]
    ));
    let line = client.recv().await;
    transcript.record("conductor -> client", &line);

    // A notification from the agent reaches the proxy wrapped.
    agent.send(
        r#"{"jsonrpc":"2.0","method":"_vendor/note","params":{"x":1,"_meta":{"inner":true}}}"#,
    );
    let line = proxy_end.recv().await;
    transcript.record("conductor -> proxy", &line);

    // A request from the agent reaches the proxy wrapped, and the proxy's
    // result reaches the agent.
    agent.send(r#"{"jsonrpc":"2.0","id":"a-1","method":"_vendor/ask","params":{"q":1}}"#);
    let (line, wrapped) = proxy_end.recv_json().await;
    transcript.record("conductor -> proxy", &line);
    proxy_end.send(&format!(
        r#"{{"jsonrpc":"2.0","id":{},"result":{{"answer":42}}}}"#,
        wrapped["id"]
    ));
    let line = agent.recv().await;
    transcript.record("conductor -> agent", &line);

    // A request from the client reaches the proxy as is; the proxy forwards
    // it to its successor and the agent's result reaches the proxy.
    client.send(r#"{"jsonrpc":"2.0","id":2,"method":"_vendor/request","params":{"y":2}}"#);
    let (line, forwarded) = proxy_end.recv_json().await;
    transcript.record("conductor -> proxy", &line);
    proxy_end.send(r#"{"jsonrpc":"2.0","id":"p-2","method":"proxy/successor","params":{"method":"_vendor/request","params":{"y":2}}}"#);
    let (line, request) = agent.recv_json().await;
    transcript.record("conductor -> agent", &line);
    agent.send(&format!(
        r#"{{"jsonrpc":"2.0","id":{},"result":{{"done":true}}}}"#,
        request["id"]
    ));
    let line = proxy_end.recv().await;
    transcript.record("conductor -> proxy", &line);
    proxy_end.send(&format!(
        r#"{{"jsonrpc":"2.0","id":{},"result":{{"done":true}}}}"#,
        forwarded["id"]
    ));
    let line = client.recv().await;
    transcript.record("conductor -> client", &line);

    // A notification from the proxy to its successor.
    proxy_end.send(r#"{"jsonrpc":"2.0","method":"proxy/successor","params":{"method":"_vendor/note","params":{"z":3}}}"#);
    let line = agent.recv().await;
    transcript.record("conductor -> agent", &line);

    task.abort();
    transcript
}

#[tokio::test(flavor = "current_thread")]
async fn v1_proxy_messages_keep_their_wire_format() {
    proxy_transcript(1).await.check(&expect![[r#"
conductor -> proxy: {"jsonrpc":"2.0","id":"<generated>","method":"proxy/initialize","params":{"protocolVersion":1,"_unknownField":true}}
conductor -> agent: {"jsonrpc":"2.0","id":"<generated>","method":"initialize","params":{"protocolVersion":1,"_unknownField":true}}
conductor -> proxy: {"jsonrpc":"2.0","id":"p-init","result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}}
conductor -> client: {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}}
conductor -> proxy: {"jsonrpc":"2.0","method":"proxy/successor","params":{"method":"_vendor/note","params":{"x":1,"_meta":{"inner":true}}}}
conductor -> proxy: {"jsonrpc":"2.0","id":"<generated>","method":"proxy/successor","params":{"method":"_vendor/ask","params":{"q":1}}}
conductor -> agent: {"jsonrpc":"2.0","id":"a-1","result":{"answer":42}}
conductor -> proxy: {"jsonrpc":"2.0","id":"<generated>","method":"_vendor/request","params":{"y":2}}
conductor -> agent: {"jsonrpc":"2.0","id":"<generated>","method":"_vendor/request","params":{"y":2}}
conductor -> proxy: {"jsonrpc":"2.0","id":"p-2","result":{"done":true}}
conductor -> client: {"jsonrpc":"2.0","id":2,"result":{"done":true}}
conductor -> agent: {"jsonrpc":"2.0","method":"_vendor/note","params":{"z":3}}
"#]]);
}

#[cfg(feature = "unstable_protocol_v2")]
#[tokio::test(flavor = "current_thread")]
async fn v2_proxy_messages_keep_their_wire_format() {
    proxy_transcript(2).await.check(&expect![[r#"
conductor -> proxy: {"jsonrpc":"2.0","id":"<generated>","method":"proxy/initialize","params":{"protocolVersion":2,"info":{"name":"raw-client","version":"1.0.0"},"_unknownField":true}}
conductor -> agent: {"jsonrpc":"2.0","id":"<generated>","method":"initialize","params":{"protocolVersion":2,"info":{"name":"raw-client","version":"1.0.0"},"_unknownField":true}}
conductor -> proxy: {"jsonrpc":"2.0","id":"p-init","result":{"protocolVersion":2,"info":{"name":"raw-agent","version":"1.0.0"}}}
conductor -> client: {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":2,"info":{"name":"raw-agent","version":"1.0.0"}}}
conductor -> proxy: {"jsonrpc":"2.0","method":"proxy/successor","params":{"method":"_vendor/note","params":{"x":1,"_meta":{"inner":true}}}}
conductor -> proxy: {"jsonrpc":"2.0","id":"<generated>","method":"proxy/successor","params":{"method":"_vendor/ask","params":{"q":1}}}
conductor -> agent: {"jsonrpc":"2.0","id":"a-1","result":{"answer":42}}
conductor -> proxy: {"jsonrpc":"2.0","id":"<generated>","method":"_vendor/request","params":{"y":2}}
conductor -> agent: {"jsonrpc":"2.0","id":"<generated>","method":"_vendor/request","params":{"y":2}}
conductor -> proxy: {"jsonrpc":"2.0","id":"p-2","result":{"done":true}}
conductor -> client: {"jsonrpc":"2.0","id":2,"result":{"done":true}}
conductor -> agent: {"jsonrpc":"2.0","method":"_vendor/note","params":{"z":3}}
"#]]);
}

/// `params` of a `proxy/successor` envelope is optional: omitted and `null`
/// both mean the wrapped message has no params. An envelope without `method`
/// is invalid.
#[tokio::test(flavor = "current_thread")]
async fn successor_envelope_params_are_optional() {
    let (proxy, mut proxy_end) = raw_component::<agent_client_protocol::Conductor>("proxy");
    let (mut client, mut agent, task) = start_chain(vec![DynConnectTo::new(proxy)]);
    let mut transcript = Transcript::default();
    let (params, result) = initialize_lines(1);

    client.send(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{params}}}"#
    ));
    let (_, init) = proxy_end.recv_json().await;
    proxy_end.send(&format!(
        r#"{{"jsonrpc":"2.0","id":"p-init","method":"proxy/successor","params":{{"method":"initialize","params":{params}}}}}"#
    ));
    let (_, request) = agent.recv_json().await;
    agent.send(&format!(
        r#"{{"jsonrpc":"2.0","id":{},"result":{result}}}"#,
        request["id"]
    ));
    proxy_end.recv().await;
    proxy_end.send(&format!(
        r#"{{"jsonrpc":"2.0","id":{},"result":{result}}}"#,
        init["id"]
    ));
    client.recv().await;

    proxy_end.send(
        r#"{"jsonrpc":"2.0","method":"proxy/successor","params":{"method":"_vendor/omitted"}}"#,
    );
    let line = agent.recv().await;
    transcript.record("conductor -> agent", &line);
    proxy_end.send(
        r#"{"jsonrpc":"2.0","method":"proxy/successor","params":{"method":"_vendor/null","params":null}}"#,
    );
    let line = agent.recv().await;
    transcript.record("conductor -> agent", &line);
    proxy_end.send(
        r#"{"jsonrpc":"2.0","id":"p-bad","method":"proxy/successor","params":{"params":{}}}"#,
    );
    let line = proxy_end.recv().await;
    transcript.record("conductor -> proxy", &line);

    task.abort();
    transcript.check(&expect![[r#"
        conductor -> agent: {"jsonrpc":"2.0","method":"_vendor/omitted"}
        conductor -> agent: {"jsonrpc":"2.0","method":"_vendor/null"}
        conductor -> proxy: {"jsonrpc":"2.0","id":"p-bad","error":{"code":-32602,"message":"Invalid params","data":{"error":"missing field `method`","json":{"params":{}},"phase":"deserialization"}}}
    "#]]);
}
