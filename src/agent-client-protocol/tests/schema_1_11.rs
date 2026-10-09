use agent_client_protocol::{ErrorCode, JsonRpcMessage, schema::v1};
use serde_json::json;

#[test]
fn stable_notices_pass_through_typed_session_notifications() {
    let capabilities = v1::ClientCapabilities::new()
        .session(v1::ClientSessionCapabilities::new().notices(v1::NoticeCapabilities::new()));
    assert_eq!(
        serde_json::to_value(capabilities).unwrap()["session"]["notices"],
        json!({})
    );

    let params = json!({
        "sessionId": "session-1",
        "update": {
            "sessionUpdate": "notice",
            "severity": "warning",
            "title": "Provider degraded",
            "description": "Retry later",
            "_meta": { "source": "provider" }
        }
    });
    let parsed = v1::AgentNotification::parse_message("session/update", &params).unwrap();
    let v1::AgentNotification::SessionNotification(notification) = &parsed else {
        panic!("expected a session notification");
    };
    assert!(matches!(notification.update, v1::SessionUpdate::Notice(_)));
    assert_eq!(parsed.to_untyped_message().unwrap().params, params);

    #[cfg(feature = "unstable_protocol_v2")]
    {
        use agent_client_protocol::schema::v2;
        let parsed = v2::AgentNotification::parse_message("session/update", &params).unwrap();
        let v2::AgentNotification::UpdateSessionNotification(notification) = &parsed else {
            panic!("expected a v2 session notification");
        };
        assert!(matches!(notification.update, v2::SessionUpdate::Notice(_)));
        assert_eq!(parsed.to_untyped_message().unwrap().params, params);
    }
}

#[test]
fn acted_on_session_setup_lists_reject_invalid_params() {
    for (field, malformed) in [
        ("additionalDirectories", json!(["/repo/lib", 42])),
        ("mcpServers", json!([{"name": "incomplete"}])),
    ] {
        let mut params = json!({ "cwd": "/repo", "mcpServers": [] });
        params[field] = malformed;
        let error = v1::ClientRequest::parse_message("session/new", &params).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidParams);

        #[cfg(feature = "unstable_protocol_v2")]
        {
            use agent_client_protocol::schema::v2;
            let error = v2::ClientRequest::parse_message("session/new", &params).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidParams);
        }
    }

    let params = json!({
        "cwd": "/repo", "mcpServers": null, "additionalDirectories": null
    });
    let parsed = v1::NewSessionRequest::parse_message("session/new", &params).unwrap();
    assert_eq!(parsed.mcp_servers, []);
    assert_eq!(
        parsed.additional_directories,
        Vec::<std::path::PathBuf>::new()
    );

    #[cfg(feature = "unstable_protocol_v2")]
    {
        use agent_client_protocol::schema::v2;
        let parsed = v2::NewSessionRequest::parse_message("session/new", &params).unwrap();
        assert_eq!(parsed.mcp_servers, []);
        assert_eq!(parsed.additional_directories, []);

        let error = v2::ResumeSessionRequest::parse_message(
            "session/resume",
            &json!({ "sessionId": "session-1", "cwd": "/repo", "replayFrom": "start" }),
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidParams);
    }
}

#[cfg(feature = "unstable_protocol_v2")]
#[test]
fn v2_error_and_custom_stop_reasons_pass_through_typed_notifications() {
    use agent_client_protocol::schema::v2;

    let reason = v2::StopReason::Error(
        v2::ErrorStopReason::new()
            .error(v2::Error::internal_error().data(json!({ "provider": "unavailable" }))),
    );
    let notification = v2::UpdateSessionNotification::new(
        "session-1",
        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
            v2::IdleStateUpdate::new().stop_reason(reason.clone()),
        )),
    );
    let mut params = notification.to_untyped_message().unwrap().params;
    assert_eq!(
        params["update"],
        json!({
            "sessionUpdate": "state_update",
            "state": "idle",
            "stopReason": "error",
            "error": {
                "code": -32603,
                "message": "Internal error",
                "data": { "provider": "unavailable" }
            }
        })
    );
    let parsed = v2::UpdateSessionNotification::parse_message("session/update", &params).unwrap();
    let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) = parsed.update else {
        panic!("expected an idle state update");
    };
    assert_eq!(idle.stop_reason, Some(reason));

    params["update"] = json!({
        "sessionUpdate": "state_update", "state": "idle",
        "stopReason": "_paused", "resumeAfter": 30,
        "_meta": { "source": "provider" }
    });
    let parsed = v2::UpdateSessionNotification::parse_message("session/update", &params).unwrap();
    assert_eq!(parsed.to_untyped_message().unwrap().params, params);

    // V1 still reports prompt failures through the JSON-RPC response error.
    assert!(
        serde_json::from_value::<v1::PromptResponse>(json!({ "stopReason": "error" })).is_err()
    );
}
