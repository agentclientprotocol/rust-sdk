//! Schema 1.17 additions parse into the SDK's typed v1 and v2 messages.

#![cfg(any(
    feature = "unstable_auth_status",
    feature = "unstable_session_options",
    feature = "unstable_subagents"
))]
#![allow(unused_imports)]

use agent_client_protocol::v1::schema as v1;
use agent_client_protocol::{JsonRpcMessage, JsonRpcResponse};
use serde_json::{Value, json};

/// Parse `params` as `method` into `E`, check the parsed method, and check
/// that the message serializes back to the same params.
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

/// Parsing `params` as `method` into `E` fails with `-32602`.
fn assert_invalid_params<E: JsonRpcMessage>(method: &str, params: &Value) {
    let error = E::parse_message(method, params)
        .err()
        .unwrap_or_else(|| panic!("{method} should reject {params}"));
    assert_eq!(i32::from(error.code), -32602, "{method} error code");
}

#[cfg(feature = "unstable_session_options")]
#[test]
fn v1_session_options_round_trips() {
    let params = json!({ "cwd": "/work", "sessionId": "sess_1" });
    assert_message_round_trips::<v1::ClientRequest>("session/options", &params);
    assert_message_round_trips::<v1::SessionOptionsRequest>("session/options", &params);
    assert_invalid_params::<v1::ClientRequest>("session/options", &json!({}));

    let result = json!({
        "configOptions": [{
            "type": "boolean",
            "id": "fast",
            "name": "Fast",
            "currentValue": false
        }]
    });
    assert_response_round_trips::<v1::AgentResponse>("session/options", &result);
    assert_response_round_trips::<v1::SessionOptionsResponse>("session/options", &result);
    assert_response_round_trips::<v1::AgentResponse>("session/options", &json!({}));
}

#[cfg(feature = "unstable_session_options")]
#[test]
fn v1_session_setup_requests_carry_initial_config_options() {
    let config_options = json!([
        { "configId": "model", "value": "model-2" },
        { "configId": "fast", "type": "boolean", "value": true }
    ]);
    for (method, mut params) in [
        ("session/new", json!({ "cwd": "/work", "mcpServers": [] })),
        (
            "session/load",
            json!({ "sessionId": "s", "cwd": "/work", "mcpServers": [] }),
        ),
        (
            "session/resume",
            json!({ "sessionId": "s", "cwd": "/work" }),
        ),
    ] {
        params["configOptions"] = config_options.clone();
        assert_message_round_trips::<v1::ClientRequest>(method, &params);

        // A malformed entry fails the request instead of being dropped.
        params["configOptions"] = json!([{ "type": "boolean", "value": true }]);
        assert_invalid_params::<v1::ClientRequest>(method, &params);
    }
}

#[cfg(feature = "unstable_subagents")]
#[test]
fn v1_session_tool_call_content_round_trips() {
    let params = json!({
        "sessionId": "parent",
        "update": {
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_1",
            "content": [{ "type": "session", "sessionId": "child" }]
        }
    });
    assert_message_round_trips::<v1::AgentNotification>("session/update", &params);
}

#[cfg(feature = "unstable_auth_status")]
#[test]
fn v1_auth_account_description_round_trips() {
    assert_response_round_trips::<v1::AgentResponse>(
        "auth/status",
        &json!({
            "state": "authenticated",
            "accounts": [{ "label": "Key", "description": "API key helper" }]
        }),
    );
}

#[cfg(feature = "unstable_protocol_v2")]
mod v2_methods {
    use agent_client_protocol::JsonRpcMessage;
    use agent_client_protocol::v2::schema as v2;
    use serde_json::json;

    use super::{assert_invalid_params, assert_message_round_trips, assert_response_round_trips};

    #[cfg(feature = "unstable_session_options")]
    #[test]
    fn v2_session_options_round_trips() {
        let params = json!({ "cwd": "/work", "sessionId": "sess_1" });
        assert_message_round_trips::<v2::ClientRequest>("session/options", &params);
        assert_message_round_trips::<v2::SessionOptionsRequest>("session/options", &params);
        assert_invalid_params::<v2::ClientRequest>("session/options", &json!({}));

        let result = json!({
            "configOptions": [{
                "type": "boolean",
                "configId": "fast",
                "name": "Fast",
                "currentValue": false
            }],
            "availableCommands": [{ "type": "command", "name": "plan", "description": "Plan" }]
        });
        assert_response_round_trips::<v2::AgentResponse>("session/options", &result);
        assert_response_round_trips::<v2::SessionOptionsResponse>("session/options", &result);
        assert_response_round_trips::<v2::AgentResponse>("session/options", &json!({}));
    }

    #[cfg(feature = "unstable_session_options")]
    #[test]
    fn v2_session_setup_requests_carry_initial_config_options() {
        for (method, mut params) in [
            ("session/new", json!({ "cwd": "/work" })),
            (
                "session/resume",
                json!({ "sessionId": "s", "cwd": "/work" }),
            ),
        ] {
            params["configOptions"] = json!([
                { "configId": "model", "type": "id", "value": "model-2" },
                { "configId": "fast", "type": "boolean", "value": true }
            ]);
            assert_message_round_trips::<v2::ClientRequest>(method, &params);

            // Entries need `type`, and a malformed entry fails the request.
            for entry in [
                json!({ "configId": "model", "value": "model-2" }),
                json!({ "configId": "fast", "type": "boolean", "value": "yes" }),
            ] {
                params["configOptions"] = json!([entry]);
                assert_invalid_params::<v2::ClientRequest>(method, &params);
            }
        }
    }

    #[cfg(feature = "unstable_subagents")]
    #[test]
    fn v2_session_tool_call_content_round_trips() {
        let mut params = json!({
            "sessionId": "parent",
            "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_1",
                "content": [{ "type": "session", "sessionId": "child" }]
            }
        });
        assert_message_round_trips::<v2::AgentNotification>("session/update", &params);

        // A malformed known variant is dropped like other malformed display
        // items. It is not read as a custom variant.
        params["update"]["content"] = json!([{ "type": "session" }]);
        let parsed = v2::AgentNotification::parse_message("session/update", &params).unwrap();
        let untyped = parsed.to_untyped_message().unwrap();
        assert_eq!(untyped.params()["update"]["content"], json!([]));
    }

    #[cfg(feature = "unstable_auth_status")]
    #[test]
    fn v2_auth_account_description_round_trips() {
        assert_response_round_trips::<v2::AgentResponse>(
            "auth/status",
            &json!({
                "state": "authenticated",
                "accounts": [{ "label": "Key", "description": "API key helper" }]
            }),
        );
    }
}
