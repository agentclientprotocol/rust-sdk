#![cfg(any(
    feature = "unstable_session_system_prompt",
    feature = "unstable_protocol_v2"
))]

#[cfg(feature = "unstable_session_system_prompt")]
use agent_client_protocol::{JsonRpcMessage, schema::v1};
use serde_json::json;

#[cfg(feature = "unstable_session_system_prompt")]
#[test]
fn system_prompts_pass_through_typed_lifecycle_requests() {
    let prompt = v1::SystemPrompt::new(vec!["Review without editing.".into()]);
    let request = v1::NewSessionRequest::new("/repo").system_prompt(prompt);
    let params = request.to_untyped_message().unwrap().params;
    assert_eq!(
        params["systemPrompt"],
        json!({
            "mode": "append",
            "content": [{ "type": "text", "text": "Review without editing." }]
        })
    );
    assert_eq!(
        v1::NewSessionRequest::parse_message("session/new", &params).unwrap(),
        request
    );

    #[cfg(feature = "unstable_protocol_v2")]
    {
        use agent_client_protocol::schema::v2;
        let prompt = v2::SystemPrompt::new(vec!["Review without editing.".into()]);
        let request = v2::ResumeSessionRequest::new("session-1", "/repo").system_prompt(prompt);
        let params = request.to_untyped_message().unwrap().params;
        assert_eq!(
            v2::ResumeSessionRequest::parse_message("session/resume", &params).unwrap(),
            request
        );
    }
}

#[cfg(feature = "unstable_session_system_prompt")]
#[test]
fn malformed_system_prompts_are_invalid_params_not_silently_omitted() {
    use agent_client_protocol::ErrorCode;

    for prompt in [
        json!("instructions"),
        json!({ "content": [] }),
        json!({ "mode": "append", "content": [{ "type": "text" }] }),
    ] {
        let params = json!({ "cwd": "/repo", "mcpServers": [], "systemPrompt": prompt });
        assert_eq!(
            v1::NewSessionRequest::parse_message("session/new", &params)
                .unwrap_err()
                .code,
            ErrorCode::INVALID_PARAMS
        );
        #[cfg(feature = "unstable_protocol_v2")]
        assert_eq!(
            agent_client_protocol::schema::v2::NewSessionRequest::parse_message(
                "session/new",
                &params
            )
            .unwrap_err()
            .code,
            ErrorCode::INVALID_PARAMS
        );
    }
}

#[cfg(feature = "unstable_protocol_v2")]
#[test]
fn v2_tool_call_metadata_updates_patch_keys_instead_of_replacing_the_object() {
    use agent_client_protocol::schema::{MaybeUndefined, v2};

    let mut stored: v2::ToolCallUpdate = serde_json::from_value(json!({
        "toolCallId": "call-1",
        "_meta": {
            "kept": true,
            "removed": 1,
            "nested": { "old": 1 }
        }
    }))
    .unwrap();
    let patch = serde_json::from_value(json!({
        "toolCallId": "call-1",
        "_meta": { "removed": null, "nested": { "new": null } }
    }))
    .unwrap();
    stored.apply_update(patch);
    let expected = json!({ "kept": true, "nested": { "new": null } });
    assert_eq!(serde_json::to_value(&stored).unwrap()["_meta"], expected);

    stored.apply_update(v2::ToolCallUpdate::new("call-1"));
    stored.apply_update(
        serde_json::from_value(json!({
            "toolCallId": "call-1", "_meta": {}
        }))
        .unwrap(),
    );
    assert_eq!(serde_json::to_value(&stored).unwrap()["_meta"], expected);

    // The helper for application-owned projections uses the same rule.
    let mut projected = MaybeUndefined::Undefined;
    v2::apply_meta_update(&mut projected, stored.meta.clone());
    assert_eq!(projected, stored.meta);
    v2::apply_meta_update(&mut projected, MaybeUndefined::Null);
    assert_eq!(projected, MaybeUndefined::Null);
}
