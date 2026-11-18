//! Schema 1.19 additions parse into the SDK's typed v1 and v2 messages.

#![allow(dead_code, unused_imports)]

use agent_client_protocol::schema::MethodDispatchError;
use agent_client_protocol::v1::schema as v1;
use agent_client_protocol::{JsonRpcMessage, JsonRpcResponse};
use serde_json::{Value, json};

/// Parse `params` as `method` into `E`, check the parsed method, and check
/// that the message serializes back to the same params.
fn assert_message_round_trips<E: JsonRpcMessage>(method: &str, params: &Value) -> E {
    let parsed = E::parse_message(method, params)
        .unwrap_or_else(|error| panic!("{method} should parse: {error:?}"));
    assert_eq!(parsed.method(), method);
    let untyped = parsed.to_untyped_message().unwrap();
    assert_eq!(untyped.method(), method);
    assert_eq!(untyped.params(), params, "{method} params round trip");
    parsed
}

/// Parsing `params` as `method` into `E` fails with `-32602`.
fn assert_invalid_params<E: JsonRpcMessage>(method: &str, params: &Value) {
    let error = E::parse_message(method, params)
        .err()
        .unwrap_or_else(|| panic!("{method} should reject {params}"));
    assert_eq!(i32::from(error.code), -32602, "{method} error code");
}

/// A `session/update` notification's params for `update`.
fn session_update(update: Value) -> Value {
    json!({ "sessionId": "sess_1", "update": update })
}

/// A `cursor_update` that announces a cursor before a user message.
fn cursor_update() -> Value {
    json!({
        "sessionUpdate": "cursor_update",
        "cursorId": "cur_1",
        "capabilities": { "fork": {} },
        "anchor": { "type": "user_message", "messageId": "msg_user_2", "position": "before" }
    })
}

/// The anchors every build with `unstable_session_cursors` understands.
fn known_anchors() -> Vec<Value> {
    vec![
        json!({ "type": "user_message", "messageId": "msg_1", "position": "before" }),
        json!({ "type": "agent_message", "messageId": "msg_2", "position": "after" }),
        json!({ "type": "agent_thought", "messageId": "msg_3", "position": "after" }),
        json!({ "type": "tool_call", "toolCallId": "call_1", "position": "after" }),
        json!({ "type": "compaction", "compactionId": "cmp_1", "position": "before" }),
    ]
}

/// Anchors that would place the cursor somewhere else if they were defaulted.
fn malformed_anchors() -> Vec<Value> {
    vec![
        json!({ "type": "user_message", "position": "before" }),
        json!({ "type": "tool_call", "toolCallId": 7, "position": "after" }),
        json!({ "type": "compaction", "compactionId": "cmp_1" }),
        json!({ "messageId": "msg_user_2", "position": "before" }),
        json!("before"),
    ]
}

/// The schema's method-aware dispatch is reachable through the SDK and agrees
/// with the SDK's own `parse_message` on which variant a method selects.
#[test]
fn v1_schema_method_dispatch_is_reachable_through_the_sdk() {
    let params = json!({ "cwd": "/work", "mcpServers": [] });
    let from_schema = v1::ClientRequest::from_method_and_params("session/new", &params).unwrap();
    let from_sdk = v1::ClientRequest::parse_message("session/new", &params).unwrap();
    assert!(matches!(
        from_schema,
        v1::ClientRequest::NewSessionRequest(_)
    ));
    assert_eq!(
        serde_json::to_value(from_schema).unwrap(),
        serde_json::to_value(from_sdk).unwrap()
    );
    assert!(v1::ClientRequest::supports_method("session/new"));

    // An unknown method is never inferred from the payload's shape.
    assert!(!v1::ClientRequest::supports_method("session/unknown"));
    assert!(matches!(
        v1::ClientRequest::from_method_and_params("session/unknown", &json!({ "sessionId": "s" })),
        Err(MethodDispatchError::UnknownMethod { method }) if method == "session/unknown"
    ));
    assert!(matches!(
        v1::ClientRequest::from_method_and_params("session/new", &json!({})),
        Err(MethodDispatchError::InvalidPayload { method, .. }) if method == "session/new"
    ));
    assert_eq!(
        i32::from(
            v1::ClientRequest::parse_message("session/unknown", &json!({}))
                .unwrap_err()
                .code
        ),
        -32601
    );
    assert_invalid_params::<v1::ClientRequest>("session/new", &json!({}));

    // A response is selected by the method of its request.
    assert!(matches!(
        v1::AgentResponse::from_method_and_result("session/new", json!({ "sessionId": "s1" })),
        Ok(v1::AgentResponse::NewSessionResponse(_))
    ));
}

#[cfg(feature = "unstable_session_cursors")]
#[test]
fn v1_cursor_update_round_trips() {
    let parsed = assert_message_round_trips::<v1::AgentNotification>(
        "session/update",
        &session_update(cursor_update()),
    );
    let v1::AgentNotification::SessionNotification(notification) = parsed else {
        panic!("expected a session notification");
    };
    let v1::SessionUpdate::CursorUpdate(update) = notification.update else {
        panic!("expected a cursor update");
    };
    assert_eq!(update.cursor_id, v1::CursorId::new("cur_1"));
    assert!(matches!(
        update.anchor,
        Some(v1::CursorAnchor::UserMessage(anchor))
            if anchor.position == v1::CursorAnchorPosition::Before
    ));

    for anchor in known_anchors() {
        assert_message_round_trips::<v1::AgentNotification>(
            "session/update",
            &session_update(json!({
                "sessionUpdate": "cursor_update",
                "cursorId": "cur_1",
                "anchor": anchor
            })),
        );
    }

    // A later update can carry the ID alone, and `null` clears the
    // capabilities.
    for update in [
        json!({ "sessionUpdate": "cursor_update", "cursorId": "cur_1" }),
        json!({ "sessionUpdate": "cursor_update", "cursorId": "cur_1", "capabilities": null }),
    ] {
        assert_message_round_trips::<v1::AgentNotification>(
            "session/update",
            &session_update(update),
        );
    }
}

/// An anchor type or position this build does not know keeps its payload, so
/// a client that stores or forwards cursors does not lose it. A client cannot
/// place such a cursor and must not guess a place for it.
#[cfg(feature = "unstable_session_cursors")]
#[test]
fn v1_cursor_update_preserves_unknown_anchors_and_rejects_malformed_ones() {
    for anchor in [
        json!({ "type": "_acme_marker", "markerId": "mk_1", "position": "after" }),
        json!({ "type": "user_message", "messageId": "msg_user_2", "position": "inside" }),
    ] {
        assert_message_round_trips::<v1::AgentNotification>(
            "session/update",
            &session_update(json!({
                "sessionUpdate": "cursor_update",
                "cursorId": "cur_1",
                "anchor": anchor
            })),
        );
    }

    // A malformed anchor fails the whole update.
    for anchor in malformed_anchors() {
        assert_invalid_params::<v1::AgentNotification>(
            "session/update",
            &session_update(json!({
                "sessionUpdate": "cursor_update",
                "cursorId": "cur_1",
                "anchor": anchor
            })),
        );
    }

    // Malformed capabilities are treated as omitted, so the client never
    // passes the cursor to a method the agent did not advertise.
    let v1::AgentNotification::SessionNotification(notification) =
        v1::AgentNotification::parse_message(
            "session/update",
            &session_update(json!({
                "sessionUpdate": "cursor_update",
                "cursorId": "cur_1",
                "capabilities": "fork"
            })),
        )
        .unwrap()
    else {
        panic!("expected a session notification");
    };
    let v1::SessionUpdate::CursorUpdate(update) = notification.update else {
        panic!("expected a cursor update");
    };
    assert!(update.capabilities.is_undefined());
}

/// In v1 the client opts in to `cursor_update` with `session.cursors`.
#[cfg(feature = "unstable_session_cursors")]
#[test]
fn v1_client_advertises_session_cursors() {
    let capabilities = v1::ClientCapabilities::new().session(
        v1::ClientSessionCapabilities::new().cursors(v1::SessionCursorsCapabilities::new()),
    );
    assert_eq!(
        serde_json::to_value(&capabilities).unwrap()["session"],
        json!({ "cursors": {} })
    );

    let params = json!({
        "protocolVersion": 1,
        "clientCapabilities": { "session": { "cursors": {} } }
    });
    let v1::ClientRequest::InitializeRequest(request) =
        v1::ClientRequest::parse_message("initialize", &params).unwrap()
    else {
        panic!("expected an initialize request");
    };
    assert!(
        request
            .client_capabilities
            .session
            .is_some_and(|session| session.cursors.is_some())
    );

    // Omitted, `null` and malformed all mean unsupported.
    for session in [
        json!({}),
        json!({ "cursors": null }),
        json!({ "cursors": 1 }),
    ] {
        let params = json!({
            "protocolVersion": 1,
            "clientCapabilities": { "session": session }
        });
        let v1::ClientRequest::InitializeRequest(request) =
            v1::ClientRequest::parse_message("initialize", &params).unwrap()
        else {
            panic!("expected an initialize request");
        };
        assert!(
            request
                .client_capabilities
                .session
                .is_none_or(|session| session.cursors.is_none())
        );
    }
}

#[cfg(all(
    feature = "unstable_session_cursors",
    feature = "unstable_session_fork"
))]
#[test]
fn v1_session_fork_carries_a_cursor_id() {
    let mut params = json!({ "sessionId": "sess_1", "cursorId": "cur_1", "cwd": "/work" });
    let parsed = assert_message_round_trips::<v1::ClientRequest>("session/fork", &params);
    let v1::ClientRequest::ForkSessionRequest(request) = parsed else {
        panic!("expected a fork request");
    };
    assert_eq!(request.cursor_id, Some(v1::CursorId::new("cur_1")));
    assert_eq!(
        v1::ForkSessionRequest::new("sess_1", "/work").cursor_id(v1::CursorId::new("cur_1")),
        request
    );

    // `null` is the same as an omitted cursor and forks the whole session.
    params["cursorId"] = Value::Null;
    let v1::ClientRequest::ForkSessionRequest(request) =
        v1::ClientRequest::parse_message("session/fork", &params).unwrap()
    else {
        panic!("expected a fork request");
    };
    assert_eq!(request.cursor_id, None);

    // A malformed cursor fails the request. It never forks the whole session.
    params["cursorId"] = json!(7);
    assert_invalid_params::<v1::ClientRequest>("session/fork", &params);
}

#[cfg(feature = "unstable_protocol_v2")]
mod v2_methods {
    use agent_client_protocol::JsonRpcMessage;
    use agent_client_protocol::v2::schema as v2;
    use serde_json::{Value, json};

    use super::{
        assert_invalid_params, assert_message_round_trips, cursor_update, known_anchors,
        malformed_anchors, session_update,
    };

    fn prompt_request(key: Value) -> Value {
        json!({
            "sessionId": "sess_1",
            "prompt": [{ "type": "text", "text": "Run the database migration" }],
            "idempotencyKey": key
        })
    }

    #[test]
    fn v2_prompt_request_carries_an_idempotency_key() {
        let key = "0b6f3c1e-8d2a-4c57-9a41-5e2f7d90c3ab";
        let parsed = assert_message_round_trips::<v2::ClientRequest>(
            "session/prompt",
            &prompt_request(json!(key)),
        );
        let v2::ClientRequest::PromptRequest(request) = parsed else {
            panic!("expected a prompt request");
        };
        assert_eq!(
            request
                .idempotency_key
                .as_ref()
                .map(v2::IdempotencyKey::as_str),
            Some(key)
        );
        // The typed request route parses the same payload.
        assert_message_round_trips::<v2::PromptRequest>(
            "session/prompt",
            &prompt_request(json!(key)),
        );

        // Omitted and `null` both mean no key, and no key is not serialized.
        let without_key = json!({
            "sessionId": "sess_1",
            "prompt": [{ "type": "text", "text": "Run the database migration" }]
        });
        let request =
            assert_message_round_trips::<v2::PromptRequest>("session/prompt", &without_key);
        assert_eq!(request.idempotency_key, None);
        let request =
            v2::PromptRequest::parse_message("session/prompt", &prompt_request(Value::Null))
                .unwrap();
        assert_eq!(request.idempotency_key, None);
        assert_eq!(serde_json::to_value(request).unwrap(), without_key);
    }

    /// The agent acts on the key, so a malformed one fails the request with
    /// `-32602` instead of being treated as omitted.
    #[test]
    fn v2_prompt_request_rejects_a_malformed_idempotency_key() {
        let too_long = "k".repeat(v2::IdempotencyKey::MAX_LENGTH + 1);
        for key in [
            json!(""),
            json!(too_long),
            json!(7),
            json!(true),
            json!(["k"]),
        ] {
            assert_invalid_params::<v2::ClientRequest>(
                "session/prompt",
                &prompt_request(key.clone()),
            );
            assert_invalid_params::<v2::PromptRequest>("session/prompt", &prompt_request(key));
        }

        // The limit counts characters, not bytes.
        let longest = "é".repeat(v2::IdempotencyKey::MAX_LENGTH);
        assert_message_round_trips::<v2::PromptRequest>(
            "session/prompt",
            &prompt_request(json!(longest)),
        );
    }

    /// `unstable_protocol_v2` enables the schema's `uuid` feature, so a v2
    /// client can generate a key without another dependency.
    #[test]
    fn v2_idempotency_keys_can_be_generated() {
        let first = v2::IdempotencyKey::generate();
        let second = v2::IdempotencyKey::generate();
        assert_ne!(first, second);
        assert_eq!(first.as_str().len(), 36);
        assert_eq!(v2::IdempotencyKey::new(first.as_str()).as_ref(), Ok(&first));

        let request =
            v2::PromptRequest::new("sess_1", vec!["hello".into()]).idempotency_key(first.clone());
        assert_eq!(
            serde_json::to_value(request).unwrap()["idempotencyKey"],
            first.as_str()
        );
    }

    /// String properties in an elicitation form no longer take `pattern`. A
    /// form that still carries it parses, and the keyword is dropped.
    #[test]
    fn v2_elicitation_string_property_has_no_pattern() {
        let params = |name: Value| {
            json!({
                "sessionId": "sess_1",
                "mode": "form",
                "message": "Name the branch",
                "requestedSchema": {
                    "type": "object",
                    "properties": { "name": name },
                    "required": ["name"]
                }
            })
        };
        let with_pattern = params(json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 64,
            "pattern": "^[a-z][a-z0-9-]*$"
        }));
        let without_pattern = params(json!({ "type": "string", "minLength": 1, "maxLength": 64 }));

        let parsed = v2::AgentRequest::parse_message("elicitation/create", &with_pattern).unwrap();
        assert_eq!(
            parsed.to_untyped_message().unwrap().params(),
            &without_pattern
        );
        assert_message_round_trips::<v2::AgentRequest>("elicitation/create", &without_pattern);
    }

    /// Agents must send at least one entry in a diff's `changes`. The SDK
    /// still parses an empty list instead of failing the whole update.
    #[test]
    fn v2_diff_with_no_changes_still_parses() {
        let params = session_update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_1",
            "content": [{ "type": "diff", "changes": [] }]
        }));
        let parsed = assert_message_round_trips::<v2::AgentNotification>("session/update", &params);
        let v2::AgentNotification::UpdateSessionNotification(notification) = parsed else {
            panic!("expected a session update notification");
        };
        let v2::SessionUpdate::ToolCallUpdate(update) = notification.update else {
            panic!("expected a tool call update");
        };
        assert!(matches!(
            update.content.value().map(Vec::as_slice),
            Some([v2::ToolCallContent::Diff(diff)]) if diff.changes.is_empty()
        ));
    }

    /// v2 needs no client capability for `cursor_update`. With
    /// `unstable_session_cursors` it is typed. Without the feature it parses
    /// as an unknown update and keeps its payload.
    #[test]
    fn v2_cursor_update_round_trips_with_or_without_the_cursors_feature() {
        let parsed = assert_message_round_trips::<v2::AgentNotification>(
            "session/update",
            &session_update(cursor_update()),
        );
        let v2::AgentNotification::UpdateSessionNotification(notification) = parsed else {
            panic!("expected a session update notification");
        };
        #[cfg(feature = "unstable_session_cursors")]
        {
            let v2::SessionUpdate::CursorUpdate(update) = notification.update else {
                panic!("expected a cursor update");
            };
            assert_eq!(update.cursor_id, v2::CursorId::new("cur_1"));
            assert!(matches!(
                update.anchor,
                Some(v2::CursorAnchor::UserMessage(_))
            ));
        }
        #[cfg(not(feature = "unstable_session_cursors"))]
        assert!(matches!(notification.update, v2::SessionUpdate::Other(_)));
    }

    #[cfg(feature = "unstable_session_cursors")]
    #[test]
    fn v2_cursor_update_covers_each_anchor_and_patches_capabilities() {
        let mut anchors = known_anchors();
        // Session-directed messages are part of v2, so this anchor needs no
        // `unstable_subagents`.
        anchors.push(json!({
            "type": "session_message",
            "messageId": "msg_session_1",
            "position": "before"
        }));
        // Unknown anchor types and positions keep their payload.
        anchors.push(json!({ "type": "_acme_marker", "markerId": "mk_1", "position": "after" }));
        anchors.push(json!({ "type": "user_message", "messageId": "msg_1", "position": "inside" }));
        for anchor in anchors {
            assert_message_round_trips::<v2::AgentNotification>(
                "session/update",
                &session_update(json!({
                    "sessionUpdate": "cursor_update",
                    "cursorId": "cur_1",
                    "anchor": anchor
                })),
            );
        }

        // A malformed anchor fails the update. It does not fall back to the
        // unknown-update variant either.
        for anchor in malformed_anchors() {
            assert_invalid_params::<v2::AgentNotification>(
                "session/update",
                &session_update(json!({
                    "sessionUpdate": "cursor_update",
                    "cursorId": "cur_1",
                    "anchor": anchor
                })),
            );
        }
        assert_invalid_params::<v2::AgentNotification>(
            "session/update",
            &session_update(json!({ "sessionUpdate": "cursor_update" })),
        );

        // `capabilities` and `_meta` are patches: omitted leaves the stored
        // value unchanged and `null` clears it.
        let parse = |update: Value| {
            let v2::AgentNotification::UpdateSessionNotification(notification) =
                assert_message_round_trips::<v2::AgentNotification>(
                    "session/update",
                    &session_update(update),
                )
            else {
                panic!("expected a session update notification");
            };
            let v2::SessionUpdate::CursorUpdate(update) = notification.update else {
                panic!("expected a cursor update");
            };
            update
        };
        let omitted = parse(json!({ "sessionUpdate": "cursor_update", "cursorId": "cur_1" }));
        assert!(omitted.capabilities.is_undefined());
        assert!(omitted.meta.is_undefined());
        assert_eq!(omitted.anchor, None);
        let cleared = parse(json!({
            "sessionUpdate": "cursor_update",
            "cursorId": "cur_1",
            "capabilities": null,
            "_meta": null
        }));
        assert!(cleared.capabilities.is_null());
        assert!(cleared.meta.is_null());
    }

    #[cfg(all(
        feature = "unstable_session_cursors",
        feature = "unstable_session_fork"
    ))]
    #[test]
    fn v2_session_fork_carries_a_cursor_id() {
        let mut params = json!({ "sessionId": "sess_1", "cursorId": "cur_1", "cwd": "/work" });
        let request = assert_message_round_trips::<v2::ForkSessionRequest>("session/fork", &params);
        assert_eq!(request.cursor_id, Some(v2::CursorId::new("cur_1")));
        assert!(matches!(
            assert_message_round_trips::<v2::ClientRequest>("session/fork", &params),
            v2::ClientRequest::ForkSessionRequest(_)
        ));

        // A cursor advertises `fork` with an object, including `{}`.
        assert_eq!(
            serde_json::to_value(
                v2::CursorCapabilities::new().fork(v2::CursorForkCapabilities::new())
            )
            .unwrap(),
            json!({ "fork": {} })
        );

        // `null` is the same as an omitted cursor and forks the whole session.
        params["cursorId"] = Value::Null;
        let request = v2::ForkSessionRequest::parse_message("session/fork", &params).unwrap();
        assert_eq!(request.cursor_id, None);

        // A malformed cursor fails the request. It never forks the whole
        // session.
        params["cursorId"] = json!(7);
        assert_invalid_params::<v2::ForkSessionRequest>("session/fork", &params);
        assert_invalid_params::<v2::ClientRequest>("session/fork", &params);
    }
}
