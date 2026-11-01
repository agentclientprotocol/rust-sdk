//! The schema crate's proxy-chain types implement the SDK's JSON-RPC traits,
//! and `SuccessorMessage` uses the schema envelope.

#![allow(unused_imports)]

use agent_client_protocol::schema::{SuccessorMessage, v1};
use agent_client_protocol::{
    ErrorCode, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, UntypedMessage,
};
use serde_json::{Value, json};

/// Compile-time check that `Req` is answered with `Resp`.
fn assert_response_type<Req: JsonRpcRequest<Response = Resp>, Resp>() {}

/// Parse `params` as `method` into the enum `E`, check the parsed method, and
/// check that the message serializes back to the same params.
fn assert_message_round_trips<E: JsonRpcMessage>(method: &str, params: &Value) {
    let parsed = E::parse_message(method, params)
        .unwrap_or_else(|error| panic!("{method} should parse: {error:?}"));
    assert_eq!(parsed.method(), method);
    let untyped = parsed.to_untyped_message().unwrap();
    assert_eq!(untyped.method(), method);
    assert_eq!(untyped.params(), params, "{method} params round trip");
}

/// Parse `result` as the response to `method` into `R` and check that it
/// serializes back to the same JSON.
fn assert_response_round_trips<R: JsonRpcResponse>(method: &str, result: &Value) {
    let parsed = R::from_json(method, result.clone())
        .unwrap_or_else(|error| panic!("{method} response should parse: {error:?}"));
    assert_eq!(
        &parsed.into_json(method).unwrap(),
        result,
        "{method} result"
    );
}

/// `proxy/initialize` carries the `initialize` params unchanged and is
/// answered with the `initialize` response unchanged.
#[test]
fn v1_initialize_proxy_round_trips() {
    // Typed parsing fills in defaults, so start from the typed form.
    let params = v1::InitializeRequest::new(agent_client_protocol::schema::ProtocolVersion::V1)
        .to_untyped_message()
        .unwrap()
        .params;
    assert_message_round_trips::<v1::InitializeProxyRequest>("proxy/initialize", &params);
    assert_message_round_trips::<v1::ProxyRequest>("proxy/initialize", &params);
    let request = v1::InitializeProxyRequest::parse_message("proxy/initialize", &params).unwrap();
    assert_eq!(
        request.to_untyped_message().unwrap().params(),
        request.initialize.to_untyped_message().unwrap().params(),
    );

    assert_response_type::<v1::InitializeProxyRequest, v1::InitializeProxyResponse>();

    let initialize =
        v1::InitializeResponse::new(agent_client_protocol::schema::ProtocolVersion::V1);
    let result = initialize.clone().into_json("initialize").unwrap();
    let response: v1::InitializeProxyResponse = initialize.clone().into();
    assert_eq!(
        response.clone().into_json("proxy/initialize").unwrap(),
        initialize.into_json("initialize").unwrap(),
    );
    assert_response_round_trips::<v1::ProxyResponse>("proxy/initialize", &result);
}

#[test]
fn v1_proxy_successor_round_trips() {
    for envelope in [
        json!({ "method": "session/prompt", "params": { "sessionId": "s", "_meta": { "inner": 1 } }, "_meta": { "outer": 1 } }),
        json!({ "method": "session/cancel" }),
    ] {
        assert_message_round_trips::<v1::ProxySuccessorRequest>("proxy/successor", &envelope);
        assert_message_round_trips::<v1::ProxySuccessorNotification>("proxy/successor", &envelope);
        assert_message_round_trips::<v1::ProxyRequest>("proxy/successor", &envelope);
        assert_message_round_trips::<v1::ProxyNotification>("proxy/successor", &envelope);
    }

    let error =
        v1::ProxySuccessorRequest::parse_message("proxy/successor", &json!({ "params": {} }))
            .unwrap_err();
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

    assert_response_type::<v1::ProxySuccessorRequest, v1::ProxySuccessorResponse>();

    // The response is the wrapped request's result, unchanged.
    let result = json!({ "zeta": 1, "alpha": [null, 1.5], "_meta": { "k": "v" } });
    assert_response_round_trips::<v1::ProxySuccessorResponse>("proxy/successor", &result);
    assert_response_round_trips::<v1::ProxyResponse>("proxy/successor", &result);
}

/// `SuccessorMessage` sends the same params as the schema envelope, and reads
/// any envelope the schema accepts.
#[test]
fn successor_message_matches_schema_envelope() {
    let prompt = v1::PromptRequest::new("s", vec![]);
    let mut meta = v1::Meta::new();
    meta.insert("outer".into(), json!(true));
    let message = SuccessorMessage::new(prompt.clone()).meta(meta.clone());
    let untyped = message.to_untyped_message().unwrap();
    assert_eq!(untyped.method(), v1::PROXY_METHOD_NAMES.proxy_successor);

    let envelope =
        v1::ProxySuccessorRequest::parse_message(untyped.method(), untyped.params()).unwrap();
    assert_eq!(&*envelope.method, "session/prompt");
    assert_eq!(envelope.meta, Some(meta.clone()));
    assert_eq!(
        serde_json::from_str::<Value>(envelope.params.as_ref().unwrap().get()).unwrap(),
        prompt.to_untyped_message().unwrap().params().clone(),
    );
    assert_eq!(
        envelope.to_untyped_message().unwrap().params(),
        untyped.params()
    );

    let parsed =
        SuccessorMessage::<v1::PromptRequest>::parse_message(untyped.method(), untyped.params())
            .unwrap();
    assert_eq!(parsed.message, prompt);
    assert_eq!(parsed.meta, Some(meta));

    assert_response_type::<SuccessorMessage<v1::PromptRequest>, v1::PromptResponse>();

    // Omitted `params` and a `_meta` that is not an object are accepted.
    let parsed = SuccessorMessage::<UntypedMessage>::parse_message(
        "proxy/successor",
        &json!({ "method": "_vendor/note", "_meta": "ignored" }),
    )
    .unwrap();
    assert_eq!(parsed.message.method(), "_vendor/note");
    assert_eq!(parsed.message.params(), &Value::Null);
    assert_eq!(parsed.meta, None);

    let error = SuccessorMessage::<UntypedMessage>::parse_message("proxy/successor", &json!({}))
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
}

#[test]
fn proxy_method_names_come_from_the_schema() {
    assert_eq!(v1::PROXY_METHOD_NAMES.proxy_initialize, "proxy/initialize");
    assert_eq!(v1::PROXY_METHOD_NAMES.proxy_successor, "proxy/successor");
    assert!(v1::InitializeProxyRequest::matches_method(
        "proxy/initialize"
    ));
    assert!(SuccessorMessage::<UntypedMessage>::matches_method(
        "proxy/successor"
    ));
}

#[cfg(feature = "unstable_protocol_v2")]
mod v2 {
    use agent_client_protocol::JsonRpcMessage;
    use agent_client_protocol::schema::v2;
    use serde_json::json;

    use super::{assert_message_round_trips, assert_response_round_trips};

    #[test]
    fn v2_proxy_types_round_trip() {
        let initialize = v2::InitializeRequest::new(
            agent_client_protocol::schema::ProtocolVersion::V2,
            v2::Implementation::new("client", "1.0.0"),
        )
        .to_untyped_message()
        .unwrap()
        .params;
        assert_message_round_trips::<v2::InitializeProxyRequest>("proxy/initialize", &initialize);
        assert_message_round_trips::<v2::ProxyRequest>("proxy/initialize", &initialize);
        let response = json!({
            "protocolVersion": 2,
            "info": { "name": "agent", "version": "1.0.0" },
            "capabilities": {},
        });
        assert_response_round_trips::<v2::InitializeProxyResponse>("proxy/initialize", &response);
        assert_response_round_trips::<v2::ProxyResponse>("proxy/initialize", &response);

        let envelope = json!({ "method": "session/prompt", "params": { "sessionId": "s" } });
        assert_message_round_trips::<v2::ProxySuccessorRequest>("proxy/successor", &envelope);
        assert_message_round_trips::<v2::ProxySuccessorNotification>("proxy/successor", &envelope);
        assert_message_round_trips::<v2::ProxyNotification>("proxy/successor", &envelope);
        assert_response_round_trips::<v2::ProxySuccessorResponse>(
            "proxy/successor",
            &json!({ "stopReason": "end_turn" }),
        );
    }
}
