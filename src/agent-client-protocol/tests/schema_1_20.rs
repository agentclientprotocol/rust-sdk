//! Schema 1.20 changes parse into the SDK's typed v1 and v2 messages.
//!
//! Plan operations and subagent sessions are stable, so the v1 tests here run
//! without any feature.

use agent_client_protocol::JsonRpcMessage;
use agent_client_protocol::schema::MaybeUndefined;
use agent_client_protocol::v1::schema as v1;
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

/// A `session/update` notification's params for `update`.
fn session_update(update: Value) -> Value {
    json!({ "sessionId": "sess_1", "update": update })
}

/// A v1 client opts in to plan operations and subagent sessions with the
/// `plan` and `subagents` capabilities. Neither needs a feature.
#[test]
fn v1_client_advertises_plan_operations_and_subagents() {
    let capabilities = v1::ClientCapabilities::new()
        .plan(v1::PlanCapabilities::new())
        .subagents(v1::SubagentCapabilities::new());
    let wire = serde_json::to_value(&capabilities).unwrap();
    assert_eq!(wire["plan"], json!({}));
    assert_eq!(wire["subagents"], json!({}));

    let parsed = v1::ClientRequest::parse_message(
        "initialize",
        &json!({
            "protocolVersion": 1,
            "clientCapabilities": { "plan": {}, "subagents": {} }
        }),
    )
    .unwrap();
    let v1::ClientRequest::InitializeRequest(initialize) = parsed else {
        panic!("expected an initialize request");
    };
    assert!(initialize.client_capabilities.plan.is_some());
    assert!(initialize.client_capabilities.subagents.is_some());

    // Omitted and `null` both mean the client did not opt in.
    for capabilities in [json!({}), json!({ "plan": null, "subagents": null })] {
        let capabilities: v1::ClientCapabilities = serde_json::from_value(capabilities).unwrap();
        assert!(capabilities.plan.is_none());
        assert!(capabilities.subagents.is_none());
    }
}

/// The v1 subagent updates are typed session updates without a feature: the
/// child's announcement with its state, and session-directed messages.
#[test]
fn v1_subagent_session_updates_are_typed_without_a_feature() {
    let announced = assert_message_round_trips::<v1::AgentNotification>(
        "session/update",
        &session_update(json!({
            "sessionUpdate": "subagent_update",
            "sessionId": "child",
            "title": "Researcher",
            "capabilities": { "cancel": {} },
            "state": { "state": "idle", "stopReason": "cancelled" }
        })),
    );
    let v1::AgentNotification::SessionNotification(notification) = announced else {
        panic!("expected a session notification");
    };
    let v1::SessionUpdate::SubagentUpdate(update) = notification.update else {
        panic!("expected a subagent update");
    };
    assert_eq!(update.session_id, v1::SessionId::new("child"));
    assert!(matches!(
        update.state,
        MaybeUndefined::Value(v1::StateUpdate::Idle(_))
    ));

    for update in [
        json!({
            "sessionUpdate": "session_message",
            "messageId": "msg_1",
            "senderSessionId": "sess_1",
            "recipientSessionId": "child"
        }),
        json!({
            "sessionUpdate": "session_message_chunk",
            "messageId": "msg_1",
            "content": { "type": "text", "text": "Please investigate" }
        }),
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_1",
            "content": [{ "type": "session", "sessionId": "child" }]
        }),
    ] {
        assert_message_round_trips::<v1::AgentNotification>(
            "session/update",
            &session_update(update),
        );
    }
}

/// An unknown state or stop reason on a v1 subagent keeps its fields in the
/// order they were sent. The schema holds them in an `IndexMap`.
#[test]
fn v1_unknown_subagent_state_keeps_its_field_order() {
    let wire = r#"{"state":"_paused","zeta":1,"alpha":2,"mid":3}"#;
    let state: v1::StateUpdate = serde_json::from_str(wire).unwrap();
    let v1::StateUpdate::Other(other) = &state else {
        panic!("expected an unknown state");
    };
    assert_eq!(
        other.fields.keys().collect::<Vec<_>>(),
        ["zeta", "alpha", "mid"]
    );
    assert_eq!(serde_json::to_string(&state).unwrap(), wire);
}

/// A cursor can be anchored to a session-directed message in v1 now that
/// subagent sessions are stable.
#[cfg(feature = "unstable_session_cursors")]
#[test]
fn v1_cursor_anchors_to_a_session_message() {
    let parsed = assert_message_round_trips::<v1::AgentNotification>(
        "session/update",
        &session_update(json!({
            "sessionUpdate": "cursor_update",
            "cursorId": "cur_1",
            "anchor": { "type": "session_message", "messageId": "msg_1", "position": "before" }
        })),
    );
    let v1::AgentNotification::SessionNotification(notification) = parsed else {
        panic!("expected a session notification");
    };
    let v1::SessionUpdate::CursorUpdate(update) = notification.update else {
        panic!("expected a cursor update");
    };
    assert!(matches!(
        update.anchor,
        Some(v1::CursorAnchor::SessionMessage(_))
    ));
}

#[cfg(feature = "unstable_protocol_v2")]
mod v2_changes {
    use std::num::NonZeroU16;

    use agent_client_protocol::JsonRpcMessage;
    use agent_client_protocol::schema::MaybeUndefined;
    use agent_client_protocol::v2::schema as v2;
    use indexmap::IndexMap;
    use serde_json::{Value, json};

    use super::{assert_message_round_trips, session_update};

    /// Parsing `params` as `method` into `E` fails with `-32602`.
    fn assert_invalid_params<E: JsonRpcMessage>(method: &str, params: &Value) {
        let error = E::parse_message(method, params)
            .err()
            .unwrap_or_else(|| panic!("{method} should reject {params}"));
        assert_eq!(i32::from(error.code), -32602, "{method} error code");
    }

    fn parse_update(update: Value) -> v2::SessionUpdate {
        let parsed = assert_message_round_trips::<v2::AgentNotification>(
            "session/update",
            &session_update(update),
        );
        let v2::AgentNotification::UpdateSessionNotification(notification) = parsed else {
            panic!("expected a session update notification");
        };
        notification.update
    }

    /// `status` on `compaction_update` is a patch field: omitted leaves it
    /// unchanged, `null` resets it to `in_progress`, and a value replaces it.
    #[test]
    fn v2_compaction_status_is_a_patch_field() {
        let v2::SessionUpdate::CompactionUpdate(summary_only) = parse_update(json!({
            "sessionUpdate": "compaction_update",
            "compactionId": "cmp_1",
            "summary": []
        })) else {
            panic!("expected a compaction update");
        };
        assert!(summary_only.status.is_undefined());

        let v2::SessionUpdate::CompactionUpdate(reset) = parse_update(json!({
            "sessionUpdate": "compaction_update",
            "compactionId": "cmp_1",
            "status": null
        })) else {
            panic!("expected a compaction update");
        };
        assert!(reset.status.is_null());

        // `new` takes only the ID, and `status` sets the patch field.
        assert_eq!(
            serde_json::to_value(v2::CompactionUpdate::new("cmp_1")).unwrap(),
            json!({ "compactionId": "cmp_1" })
        );
        assert_eq!(
            serde_json::to_value(
                v2::CompactionUpdate::new("cmp_1").status(v2::CompactionStatus::Completed)
            )
            .unwrap(),
            json!({ "compactionId": "cmp_1", "status": "completed" })
        );
    }

    /// Top-level `_meta` on `usage_update` is a metadata patch, applied with
    /// `apply_meta_update` like the `_meta` of the other upsert updates.
    #[test]
    fn v2_usage_update_meta_is_a_metadata_patch() {
        let usage = |meta: Option<Value>| {
            let mut update = json!({ "sessionUpdate": "usage_update", "used": 1, "size": 2 });
            if let Some(meta) = meta {
                update["_meta"] = meta;
            }
            let v2::SessionUpdate::UsageUpdate(update) = parse_update(update) else {
                panic!("expected a usage update");
            };
            update
        };
        assert!(usage(None).meta.is_undefined());
        assert!(usage(Some(json!(null))).meta.is_null());

        let mut stored = MaybeUndefined::Undefined;
        v2::apply_meta_update(
            &mut stored,
            usage(Some(json!({ "com.example/a": 1, "com.example/b": 2 }))).meta,
        );
        // A key set to `null` is removed and the other keys are kept.
        v2::apply_meta_update(
            &mut stored,
            usage(Some(json!({ "com.example/a": null }))).meta,
        );
        // An update without `_meta` leaves the stored metadata unchanged.
        v2::apply_meta_update(&mut stored, usage(None).meta);
        assert_eq!(
            serde_json::to_value(&stored).unwrap(),
            json!({ "com.example/b": 2 })
        );
    }

    /// Markdown plans and `plan_removed` are part of v2 without a feature.
    #[test]
    fn v2_plan_operations_are_typed_without_a_feature() {
        assert!(matches!(
            parse_update(json!({
                "sessionUpdate": "plan_update",
                "plan": {
                    "type": "markdown",
                    "planId": "plan-1",
                    "content": "## Steps",
                    "path": "/workspace/PLAN.md"
                }
            })),
            v2::SessionUpdate::PlanUpdate(v2::PlanUpdate {
                plan: v2::PlanUpdateContent::Markdown(_),
                ..
            })
        ));
        assert!(matches!(
            parse_update(json!({ "sessionUpdate": "plan_removed", "planId": "plan-1" })),
            v2::SessionUpdate::PlanRemoved(_)
        ));
        // A known plan type with a missing field fails instead of parsing as
        // an unknown plan type.
        assert_invalid_params::<v2::AgentNotification>(
            "session/update",
            &session_update(json!({
                "sessionUpdate": "plan_update",
                "plan": { "type": "markdown", "planId": "plan-1" }
            })),
        );
    }

    /// A permission request that offers nothing to select cannot be answered,
    /// so it fails typed parsing and the SDK answers it with `-32602`.
    #[test]
    fn v2_permission_request_needs_at_least_one_option() {
        let request = |options: Value| {
            json!({
                "sessionId": "sess_1",
                "title": "Run the migration?",
                "options": options
            })
        };
        assert_message_round_trips::<v2::AgentRequest>(
            "session/request_permission",
            &request(json!([
                { "optionId": "reject", "name": "Reject", "kind": "reject_once" }
            ])),
        );
        assert_invalid_params::<v2::AgentRequest>(
            "session/request_permission",
            &request(json!([])),
        );
    }

    /// `SystemPrompt.content` has `minItems: 1` in the JSON Schema. The typed
    /// request still parses an empty list, so an agent that wants to reject
    /// one checks for it.
    #[test]
    fn v2_empty_system_prompt_content_still_parses() {
        let parsed = assert_message_round_trips::<v2::ClientRequest>(
            "session/new",
            &json!({
                "cwd": "/work",
                "systemPrompt": { "mode": "append", "content": [] }
            }),
        );
        let v2::ClientRequest::NewSessionRequest(request) = parsed else {
            panic!("expected a new session request");
        };
        assert_eq!(
            request.system_prompt.unwrap().content,
            Vec::<v2::ContentBlock>::new()
        );
    }

    /// An elicitation names a session or a pending request, never both.
    /// `toolCallId` is only valid with `sessionId`, and the unused scope field
    /// is omitted, not `null`.
    #[test]
    fn v2_elicitation_scope_is_exclusive() {
        let request = |scope: Value| {
            let mut request = json!({
                "mode": "url",
                "message": "Sign in",
                "elicitationId": "elicit_1",
                "url": "https://example.com/login"
            });
            request
                .as_object_mut()
                .unwrap()
                .extend(scope.as_object().unwrap().clone());
            request
        };
        for scope in [
            json!({ "sessionId": "sess_1" }),
            json!({ "sessionId": "sess_1", "toolCallId": "call_1" }),
            json!({ "requestId": 7 }),
        ] {
            assert_message_round_trips::<v2::AgentRequest>("elicitation/create", &request(scope));
        }
        for scope in [
            json!({ "sessionId": "sess_1", "requestId": 7 }),
            json!({ "sessionId": "sess_1", "requestId": null }),
            json!({ "sessionId": null, "requestId": 7 }),
            json!({ "requestId": 7, "toolCallId": "call_1" }),
            json!({ "requestId": 7, "toolCallId": null }),
            json!({}),
        ] {
            assert_invalid_params::<v2::AgentRequest>("elicitation/create", &request(scope));
        }
    }

    /// In multi-select `items`, the titled `anyOf` form has no `type`. A
    /// `type` of `"string"` needs `enum`, so it does not parse as the titled
    /// form when `anyOf` is also present.
    #[test]
    fn v2_multi_select_items_are_selected_by_type() {
        let titled = json!({ "anyOf": [{ "const": "red", "title": "Red" }] });
        assert!(matches!(
            serde_json::from_value::<v2::MultiSelectItems>(titled).unwrap(),
            v2::MultiSelectItems::Titled(_)
        ));
        let untitled = json!({ "type": "string", "enum": ["red"] });
        assert!(matches!(
            serde_json::from_value::<v2::MultiSelectItems>(untitled).unwrap(),
            v2::MultiSelectItems::String(_)
        ));
        let mixed = json!({ "type": "string", "anyOf": [{ "const": "red", "title": "Red" }] });
        assert!(serde_json::from_value::<v2::MultiSelectItems>(mixed).is_err());
    }

    /// A terminal has at least one column and one row. A size of zero is not
    /// a size, and the rest of the `terminal_update` still applies.
    #[test]
    fn v2_terminal_size_is_nonzero() {
        let size =
            v2::TerminalSize::new(NonZeroU16::new(120).unwrap(), NonZeroU16::new(40).unwrap());
        assert_eq!(
            serde_json::to_value(size).unwrap(),
            json!({ "cols": 120, "rows": 40 })
        );
        assert!(
            serde_json::from_value::<v2::TerminalSize>(json!({ "cols": 0, "rows": 40 })).is_err()
        );

        let update: v2::TerminalUpdate = serde_json::from_value(json!({
            "terminalId": "term_1",
            "command": "cargo test",
            "size": { "cols": 0, "rows": 40 }
        }))
        .unwrap();
        assert!(update.size.is_undefined());
        assert!(update.command.is_value());
    }

    /// Maps on the wire keep the order they were sent or built in: the fields
    /// of an unknown variant, the properties of an elicitation form, and the
    /// content of an accepted form.
    #[test]
    fn v2_maps_keep_their_key_order() {
        let block = r#"{"type":"_custom","zeta":1,"alpha":2,"mid":3}"#;
        let parsed: v2::ContentBlock = serde_json::from_str(block).unwrap();
        assert_eq!(serde_json::to_string(&parsed).unwrap(), block);

        // An unknown variant is built from an `IndexMap`.
        let built = v2::ContentBlock::Other(v2::OtherContentBlock::new(
            "_custom",
            IndexMap::from([
                ("zeta".to_string(), json!(1)),
                ("alpha".to_string(), json!(2)),
                ("mid".to_string(), json!(3)),
            ]),
        ));
        assert_eq!(serde_json::to_string(&built).unwrap(), block);

        let form = r#"{"type":"object","properties":{"zeta":{"type":"string"},"alpha":{"type":"boolean"}}}"#;
        let schema: v2::ElicitationSchema = serde_json::from_str(form).unwrap();
        assert_eq!(
            schema.properties.keys().collect::<Vec<_>>(),
            ["zeta", "alpha"]
        );
        assert_eq!(serde_json::to_string(&schema).unwrap(), form);

        let accepted = r#"{"action":"accept","content":{"zeta":"z","alpha":true}}"#;
        let response: v2::CreateElicitationResponse = serde_json::from_str(accepted).unwrap();
        assert_eq!(serde_json::to_string(&response).unwrap(), accepted);
    }
}
