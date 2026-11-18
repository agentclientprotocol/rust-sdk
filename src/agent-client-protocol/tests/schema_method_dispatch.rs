//! SDK adapters preserve handler matching and error policy around schema parsing.

use agent_client_protocol::{Error, ErrorCode, JsonRpcMessage, JsonRpcResponse};
use serde_json::{Value, json};

fn assert_same_error(actual: Error, expected: Error) {
    assert_eq!(actual.code, expected.code);
    assert_eq!(actual.message, expected.message);
    assert_eq!(actual.data, expected.data);
}

macro_rules! adapter_tests {
    () => {
        #[test]
        fn sdk_routes_requests_and_results_by_method() {
            let params = json!({"cwd": "/work", "mcpServers": []});
            let request = ClientRequest::parse_message("session/new", &params).unwrap();
            assert!(matches!(request, ClientRequest::NewSessionRequest(_)));
            assert_eq!(request.to_untyped_message().unwrap().method(), "session/new");
            assert_eq!(
                serde_json::to_value(&request).unwrap(),
                serde_json::to_value(
                    ClientRequest::from_method_and_params("session/new", &params).unwrap()
                ).unwrap()
            );
            let result = json!({"sessionId": "s1"});
            let response = AgentResponse::from_json("session/new", result.clone()).unwrap();
            assert!(matches!(response, AgentResponse::NewSessionResponse(_)));
            assert_eq!(response.into_json("session/new").unwrap(), result);
        }

        #[test]
        fn sdk_preserves_request_and_response_error_diagnostics() {
            for payload in [Value::Null, json!({}), json!(42)] {
                assert_same_error(
                    ClientRequest::parse_message("session/new", &payload).unwrap_err(),
                    NewSessionRequest::parse_message("session/new", &payload).unwrap_err(),
                );
                assert_same_error(
                    AgentResponse::from_json("session/new", payload.clone()).unwrap_err(),
                    NewSessionResponse::from_json("session/new", payload).unwrap_err(),
                );
            }
            let error = ClientRequest::parse_message("session/new", &json!({})).unwrap_err();
            assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
            let error = AgentResponse::from_json("session/new", json!({})).unwrap_err();
            assert_eq!(error.code, ErrorCode::PARSE_ERROR);

            let error = AgentNotification::parse_message("session/update", &json!({}))
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
            let error = ClientNotification::parse_message("session/cancel", &json!({}))
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
            let error = ProtocolLevelNotification::parse_message("$/cancel_request", &json!({}))
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
        }

        #[test]
        fn sdk_keeps_catch_all_and_protocol_handler_matching_distinct() {
            // Schema method recognition must not shadow or narrow the SDK's
            // catch-all handler matching.
            assert!(ClientRequest::matches_method("unknown"));
            assert!(AgentRequest::matches_method("unknown"));
            assert!(ClientNotification::matches_method("unknown"));
            assert!(AgentNotification::matches_method("unknown"));
            assert!(ProtocolLevelNotification::matches_method("$/cancel_request"));
            assert!(!ProtocolLevelNotification::matches_method("unknown"));
            assert!(!ProtocolLevelNotification::matches_method("_vendor/custom"));

            for error in [
                ClientRequest::parse_message("unknown", &json!({})).unwrap_err(),
                AgentRequest::parse_message("unknown", &json!({})).unwrap_err(),
                ClientNotification::parse_message("unknown", &json!({})).unwrap_err(),
                AgentNotification::parse_message("unknown", &json!({})).unwrap_err(),
                ProtocolLevelNotification::parse_message("unknown", &json!({})).unwrap_err(),
                AgentResponse::from_json("unknown", json!({})).unwrap_err(),
                ClientResponse::from_json("unknown", json!({})).unwrap_err(),
            ] {
                assert_eq!(error.code, ErrorCode::METHOD_NOT_FOUND);
                assert_eq!(error.data, None);
            }
        }

        #[test]
        fn sdk_extensions_keep_method_and_payload_in_both_directions() {
            let method = "_vendor/custom";
            for params in [Value::Null, json!({"payload": [1, null]}), json!([1, 2])] {
                macro_rules! message {
                    ($enum:ident) => {
                        let parsed = $enum::parse_message(method, &params).unwrap();
                        let untyped = parsed.to_untyped_message().unwrap();
                        assert_eq!(untyped.method(), method);
                        assert_eq!(untyped.params(), &params);
                    };
                }
                message!(ClientRequest);
                message!(AgentRequest);
                message!(ClientNotification);
                message!(AgentNotification);
                assert_eq!(
                    AgentResponse::from_json(method, params.clone()).unwrap()
                        .into_json(method).unwrap(),
                    params,
                );
                assert_eq!(
                    ClientResponse::from_json(method, params.clone()).unwrap()
                        .into_json(method).unwrap(),
                    params,
                );
            }
        }
    };
}

mod v1 {
    use super::*;
    use agent_client_protocol::v1::schema::{
        AgentNotification, AgentRequest, AgentResponse, ClientNotification, ClientRequest,
        ClientResponse, NewSessionRequest, NewSessionResponse, ProtocolLevelNotification,
    };

    adapter_tests!();

    #[tokio::test(flavor = "current_thread")]
    async fn known_request_without_a_handler_returns_method_not_found() -> Result<(), Error> {
        use agent_client_protocol::{Agent, Client};
        use std::time::Duration;

        let request = ClientRequest::from_method_and_params(
            "session/new",
            &json!({"cwd": "/work", "mcpServers": []}),
        )
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(10),
            Client.v1().connect_with(Agent.v1(), async move |cx| {
                let error = cx
                    .send_request(request)
                    .block_task()
                    .await
                    .expect_err("the agent has no session/new handler");
                assert_eq!(error.code, ErrorCode::METHOD_NOT_FOUND);
                Ok(())
            }),
        )
        .await
        .expect("the agent should answer the unhandled request")
    }
}

#[cfg(feature = "unstable_protocol_v2")]
mod v2 {
    use super::*;
    use agent_client_protocol::v2::schema::{
        AgentNotification, AgentRequest, AgentResponse, ClientNotification, ClientRequest,
        ClientResponse, NewSessionRequest, NewSessionResponse, ProtocolLevelNotification,
    };

    adapter_tests!();
}
