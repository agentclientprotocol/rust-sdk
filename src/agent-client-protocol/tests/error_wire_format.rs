//! Pins the JSON-RPC error objects that v1 and v2 connections put on the wire.
//!
//! Each test drives an agent over a raw [`Channel`], so the snapshots show
//! exactly what a peer receives: errors built by handlers, errors the SDK
//! generates itself (unknown methods, dropped responders), and initialization
//! failures.

use agent_client_protocol::RawJsonRpcId;
use agent_client_protocol::schema::v1;
use agent_client_protocol::{
    Agent, Client, ConnectTo, Error, JsonRpcRequest, JsonRpcResponse, RawJsonRpcMessage,
    TransportFrame,
};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_test/fail", response = FailResponse)]
struct FailRequest {
    case: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
struct FailResponse {}

/// Error cases whose wire form is the same on v1 and v2 connections.
const SHARED_CASES: &[&str] = &[
    "parse_error",
    "invalid_request",
    "method_not_found",
    "invalid_params",
    "internal_error",
    "request_cancelled",
    "auth_required",
    "session_not_found",
    "resource_not_found",
    "into_internal_error",
    "serde_json",
    "custom_code",
    "null_data",
    "dropped",
];

/// Builds the error a handler returns for `case`, with no protocol-specific
/// shape. `None` means the handler drops the responder instead.
fn shared_error(case: &str) -> Option<Error> {
    Some(match case {
        "parse_error" => Error::parse_error(),
        "invalid_request" => Error::invalid_request(),
        "method_not_found" => Error::method_not_found(),
        "invalid_params" => Error::invalid_params().data(json!({ "field": "timeout" })),
        "internal_error" => Error::internal_error().data("details"),
        "request_cancelled" => Error::request_cancelled(),
        "auth_required" => Error::auth_required(),
        "into_internal_error" => Error::into_internal_error(std::io::Error::other("disk on fire")),
        "serde_json" => Error::from(serde_json::from_str::<u32>("\"x\"").unwrap_err()),
        "custom_code" => Error::new(12345, "custom").data(json!([1, { "nested": null }])),
        "null_data" => Error::internal_error().data(Value::Null),
        "dropped" => return None,
        other => panic!("unknown case {other}"),
    })
}

fn v1_error(case: &str) -> Option<Error> {
    match case {
        "session_not_found" => Some(v1::Error::session_not_found("sess_1").into()),
        "resource_not_found" => {
            Some(v1::Error::resource_not_found(Some("file:///tmp/missing".into())).into())
        }
        case => shared_error(case),
    }
}

fn respond<P: agent_client_protocol::Protocol>(
    responder: agent_client_protocol::Responder<FailResponse, P>,
    error: Option<Error>,
) {
    match error {
        Some(error) => responder.respond_with_error(error).unwrap(),
        None => drop(responder),
    }
}

/// Send each request in turn and collect the response it gets.
async fn exchange(agent: impl ConnectTo<Client> + 'static, requests: Vec<(&str, Value)>) -> Value {
    let (mut channel, driver) = ConnectTo::<Client>::into_channel_and_future(agent);
    let task = tokio::spawn(driver.expect("agent owns a connection driver"));
    let mut responses = Vec::new();
    for (id, (method, params)) in requests.into_iter().enumerate() {
        channel
            .tx
            .unbounded_send(TransportFrame::Single(
                RawJsonRpcMessage::request(
                    method.into(),
                    params,
                    RawJsonRpcId::Number(i64::try_from(id).unwrap()),
                )
                .unwrap(),
            ))
            .unwrap();
        loop {
            let frame = channel
                .rx
                .next()
                .await
                .expect("agent answers every request");
            if let TransportFrame::Single(message @ RawJsonRpcMessage::Response(_)) = frame {
                let mut response = serde_json::to_value(message).unwrap();
                // Successful initialize results are not under test.
                if let Some(result) = response.get_mut("result") {
                    *result = json!("<ok>");
                }
                responses.push(response);
                break;
            }
        }
    }
    task.abort();
    Value::Array(responses)
}

fn fail_requests(cases: &[&str]) -> Vec<(&'static str, Value)> {
    cases
        .iter()
        .map(|case| ("_test/fail", json!({ "case": case })))
        .collect()
}

fn check(responses: &Value, expected: Value) {
    assert_eq!(
        responses,
        &expected,
        "wire format changed:\n{}",
        serde_json::to_string_pretty(responses).unwrap()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn v1_connection_error_wire_format() {
    let agent = Agent
        .v1()
        .on_receive_request(
            async |request: FailRequest, responder, _cx| {
                respond(responder, v1_error(&request.case));
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: v1::InitializeRequest, responder, _cx| {
                responder.respond(v1::InitializeResponse::new(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        );

    let mut requests = vec![(
        "initialize",
        serde_json::to_value(v1::InitializeRequest::new(
            agent_client_protocol::schema::ProtocolVersion::V1,
        ))
        .unwrap(),
    )];
    requests.extend(fail_requests(SHARED_CASES));
    requests.push(("_test/unknown", json!({})));

    check(&exchange(agent, requests).await, v1_expected());
}

fn v1_expected() -> Value {
    json!([
        {"jsonrpc": "2.0", "id": 0, "result": "<ok>"},
        {"jsonrpc": "2.0", "id": 1, "error": {"code": -32700, "message": "Parse error"}},
        {"jsonrpc": "2.0", "id": 2, "error": {"code": -32600, "message": "Invalid request"}},
        {"jsonrpc": "2.0", "id": 3, "error": {"code": -32601, "message": "Method not found"}},
        {"jsonrpc": "2.0", "id": 4, "error": {"code": -32602, "message": "Invalid params", "data": {"field": "timeout"}}},
        {"jsonrpc": "2.0", "id": 5, "error": {"code": -32603, "message": "Internal error", "data": "details"}},
        {"jsonrpc": "2.0", "id": 6, "error": {"code": -32800, "message": "Request cancelled"}},
        {"jsonrpc": "2.0", "id": 7, "error": {"code": -32000, "message": "Authentication required"}},
        {"jsonrpc": "2.0", "id": 8, "error": {"code": -32002, "message": "Resource not found", "data": {"sessionId": "sess_1"}}},
        {"jsonrpc": "2.0", "id": 9, "error": {"code": -32002, "message": "Resource not found", "data": {"uri": "file:///tmp/missing"}}},
        {"jsonrpc": "2.0", "id": 10, "error": {"code": -32603, "message": "Internal error", "data": "disk on fire"}},
        {"jsonrpc": "2.0", "id": 11, "error": {"code": -32602, "message": "Invalid params", "data": "invalid type: string \"x\", expected u32 at line 1 column 3"}},
        {"jsonrpc": "2.0", "id": 12, "error": {"code": 12345, "message": "custom", "data": [1, {"nested": null}]}},
        {"jsonrpc": "2.0", "id": 13, "error": {"code": -32603, "message": "Internal error", "data": null}},
        {"jsonrpc": "2.0", "id": 14, "error": {"code": -32603, "message": "Internal error", "data": "request handler dropped its responder for `_test/fail` without responding"}},
        {"jsonrpc": "2.0", "id": 15, "error": {"code": -32601, "message": "Method not found", "data": "_test/unknown"}},
    ])
}

#[cfg(feature = "unstable_protocol_v2")]
mod v2_connection {
    use agent_client_protocol::schema::{ProtocolVersion, v2};
    use agent_client_protocol::{Agent, AgentProtocolRouter, Error};
    use serde_json::{Value, json};

    use super::{FailRequest, SHARED_CASES, check, exchange, fail_requests, respond, shared_error};

    fn v2_error(case: &str) -> Option<Error> {
        match case {
            "session_not_found" => Some(v2::Error::session_not_found("sess_1").into()),
            "resource_not_found" => Some(v2::Error::resource_not_found().into()),
            case => shared_error(case),
        }
    }

    fn v2_agent() -> impl agent_client_protocol::ConnectTo<agent_client_protocol::Client> {
        Agent
            .v2()
            .on_receive_request(
                async |request: FailRequest, responder, _cx| {
                    respond(responder, v2_error(&request.case));
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async |request: v2::InitializeRequest, responder, _cx| {
                    responder.respond(v2::InitializeResponse::new(
                        request.protocol_version,
                        v2::Implementation::new("agent", "1.0.0"),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
    }

    fn v2_initialize(version: ProtocolVersion) -> serde_json::Value {
        serde_json::to_value(v2::InitializeRequest::new(
            version,
            v2::Implementation::new("client", "1.0.0"),
        ))
        .unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn v2_connection_error_wire_format() {
        let mut requests = vec![
            // A v1 initialize is refused with -32022 and leaves the
            // connection uninitialized, so the next initialize can succeed.
            ("initialize", v2_initialize(ProtocolVersion::V1)),
            ("initialize", json!({ "protocolVersion": "one" })),
            ("initialize", v2_initialize(ProtocolVersion::V2)),
        ];
        requests.extend(fail_requests(SHARED_CASES));
        requests.push(("_test/unknown", json!({})));

        check(&exchange(v2_agent(), requests).await, v2_expected());
    }

    fn v2_expected() -> Value {
        json!([
            {"jsonrpc": "2.0", "id": 0, "error": {"code": -32022, "message": "Unsupported protocol version", "data": {"requested": 1, "supported": [2]}}},
            {"jsonrpc": "2.0", "id": 1, "error": {"code": -32602, "message": "Invalid params", "data": "initialize.protocolVersion must be a valid ACP protocol version"}},
            {"jsonrpc": "2.0", "id": 2, "result": "<ok>"},
            {"jsonrpc": "2.0", "id": 3, "error": {"code": -32700, "message": "Parse error"}},
            {"jsonrpc": "2.0", "id": 4, "error": {"code": -32600, "message": "Invalid request"}},
            {"jsonrpc": "2.0", "id": 5, "error": {"code": -32601, "message": "Method not found"}},
            {"jsonrpc": "2.0", "id": 6, "error": {"code": -32602, "message": "Invalid params", "data": {"field": "timeout"}}},
            {"jsonrpc": "2.0", "id": 7, "error": {"code": -32603, "message": "Internal error", "data": "details"}},
            {"jsonrpc": "2.0", "id": 8, "error": {"code": -32800, "message": "Request cancelled"}},
            {"jsonrpc": "2.0", "id": 9, "error": {"code": -32000, "message": "Authentication required"}},
            {"jsonrpc": "2.0", "id": 10, "error": {"code": -32002, "message": "Resource not found", "data": {"sessionId": "sess_1"}}},
            {"jsonrpc": "2.0", "id": 11, "error": {"code": -32002, "message": "Resource not found"}},
            {"jsonrpc": "2.0", "id": 12, "error": {"code": -32603, "message": "Internal error", "data": "disk on fire"}},
            {"jsonrpc": "2.0", "id": 13, "error": {"code": -32602, "message": "Invalid params", "data": "invalid type: string \"x\", expected u32 at line 1 column 3"}},
            {"jsonrpc": "2.0", "id": 14, "error": {"code": 12345, "message": "custom", "data": [1, {"nested": null}]}},
            {"jsonrpc": "2.0", "id": 15, "error": {"code": -32603, "message": "Internal error", "data": null}},
            {"jsonrpc": "2.0", "id": 16, "error": {"code": -32603, "message": "Internal error", "data": "request handler dropped its responder for `_test/fail` without responding"}},
            {"jsonrpc": "2.0", "id": 17, "error": {"code": -32601, "message": "Method not found", "data": "_test/unknown"}},
        ])
    }

    /// A router with only a v2 implementation answers a v1 client's
    /// `initialize` with -32022.
    #[tokio::test(flavor = "current_thread")]
    async fn v2_only_router_rejects_v1_client_wire_format() {
        let router = AgentProtocolRouter::new().with_v2(v2_agent());
        let requests = vec![
            (
                "initialize",
                serde_json::to_value(agent_client_protocol::schema::v1::InitializeRequest::new(
                    ProtocolVersion::V1,
                ))
                .unwrap(),
            ),
            ("_test/fail", json!({ "case": "auth_required" })),
        ];

        check(&exchange(router, requests).await, router_expected());
    }

    fn router_expected() -> Value {
        json!([
            {"jsonrpc": "2.0", "id": 0, "error": {"code": -32022, "message": "Unsupported protocol version", "data": {"requested": 1, "supported": [2]}}},
            {"jsonrpc": "2.0", "id": 1, "error": {"code": -32600, "message": "Invalid request", "data": "first ACP request must be `initialize`"}},
        ])
    }
}
